use anyhow::{anyhow, Result};
use quick_xml::{events::Event, Reader};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env::current_dir;
use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
use std::fs::{self, create_dir, read_to_string, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Debug)]
struct Package {
    name: String,
    depend: Vec<String>,
    build_depend: Vec<String>,
    build_export_depend: Vec<String>,
    exec_depend: Vec<String>,
    test_depend: Vec<String>,
    buildtool_depend: Vec<String>,
    doc_depend: Vec<String>,
    run_depend: Vec<String>,
}

fn bloom(pkg: &str, package_dir: &Path, path: &Path) -> Result<()> {
    let build_root = tempdir()?;
    println!("{build_root:?}");

    let pb = build_root.path().join("build");
    create_dir(&pb)?;

    let cwd = current_dir()?;
    let p = cwd.join(path);

    println!("{p:?}");

    // Generate debian build directory

    let o = Command::new("bloom-generate")
        .args([
            "rosdebian",
            "--os-name",
            "ubuntu",
            "--os-version",
            "bionic",
            "--ros-distro",
            "melodic",
        ])
        .arg(&p)
        .current_dir(&pb)
        .output()?;

    println!(
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );

    // Patch debian/rules to use the correct package path

    let rules_path = pb.join("debian/rules");
    let rules = read_to_string(&rules_path)?.replace(
        "$(BUILD_TESTING_ARG)",
        &format!("{} $(BUILD_TESTING_ARG)", p.display()),
    );

    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(rules_path)?;
    f.write_all(rules.as_bytes())?;
    std::mem::drop(f);

    // Generate binary

    let o = Command::new("fakeroot")
        .args(["debian/rules", "binary"])
        .current_dir(&pb)
        .output()?;

    if o.status.code().unwrap_or_default() == 0 {
        println!(
            "stdout:\n{}\n\nstderr:\n{}",
            "", //String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    } else {
        println!(
            "stdout:\n{}\n\nstderr:\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        return Err(anyhow!("Failed to do {pkg}"));
    }

    // Copy the generated debs out and update the package list

    let mut packages = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .append(true)
        .open(package_dir.join("Packages"))?;

    let o = Command::new("dpkg-scanpackages")
        .args(["-m", "."])
        .current_dir(&build_root)
        .output()?;

    for fp in String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(str::trim)
        .filter_map(|s| s.strip_prefix("Filename: "))
        .map(Path::new)
    {
        println!("{fp:?}");
        let origin = build_root.path().join(fp);
        let target = package_dir.join(fp);
        println!("DEB: {}", target.display());
        fs::copy(&origin, &target)?;

        let o = Command::new("dpkg").args(["-i"]).arg(&target).output()?;

        println!(
            "stdout:\n{}\n\nstderr:\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    }

    packages.write(&o.stdout)?;

    Ok(())
}

fn main() -> Result<()> {
    println!("Hello, world!");

    let mut pkgs = HashMap::new();
    let mut workspace_pkgs = HashSet::new();

    let ignored_pkgs: &[&str] = &[
        "champ_gazebo", // ignored
    ];

    let checked_pkgs: &[&str] = &[];
    let pkgs_to_check: &[&str] = &[];

    // Step 1 - collect all dependencies in the workspace
    for entry in WalkDir::new(".") {
        if let Ok(entry) = entry {
            if entry.file_type().is_file() && entry.file_name() == OsStr::new("package.xml") {
                println!("{}", entry.path().display());

                let mut reader = Reader::from_file(entry.path())?;
                let mut buf = vec![];

                let mut name = None;
                let mut depends = HashSet::new();

                loop {
                    match reader.read_event(&mut buf)? {
                        Event::Start(ref e) if e.name() == b"name" => {
                            name = reader.read_text(e.name(), &mut vec![]).ok();
                        }
                        Event::Start(ref e) if e.name().ends_with(b"depend") => {
                            let dep = reader.read_text(e.name(), &mut vec![]).unwrap_or_default();
                            depends.insert(dep);
                        }
                        Event::Eof => break,
                        _ => {}
                    }

                    buf.clear();
                }

                if let Some(name) = name {
                    if !ignored_pkgs.contains(&name.as_str()) {
                        workspace_pkgs.insert(name.clone());
                        let mut dir = entry.into_path();
                        dir.pop();
                        pkgs.insert(name, (dir, depends));
                    }
                }
            }
        }
    }

    // Step 2 - clear out any non-workspace deps
    for (_, deps) in pkgs.values_mut() {
        deps.retain(|v| workspace_pkgs.contains(v));
    }

    println!("{pkgs:?}");

    // Step 3 - sort the packages in the dependency fullfilling order
    let mut ordered_pkgs = vec![];

    let mut tmp_pkgs = pkgs
        .iter()
        .map(|(n, (p, d))| (n.clone(), p.clone(), d.clone()))
        .collect::<Vec<_>>();

    for o in 0.. {
        let mut drained = vec![];
        let mut drained_names = HashSet::new();

        let mut i = 0;
        while i < tmp_pkgs.len() {
            if tmp_pkgs[i].2.is_empty() {
                println!("REMOVE {}", tmp_pkgs[i].0);
                let (name, path, deps) = tmp_pkgs.swap_remove(i);
                drained_names.insert(name.clone());
                let pkg = format!("ros-melodic-{}", name.replace("_", "-"));
                drained.push((name, pkg, path, deps, o));
            } else {
                i += 1;
            }
        }

        if drained.is_empty() {
            break;
        }

        println!("DRAIN {:?}", drained);

        for (_, _, d) in &mut tmp_pkgs {
            d.retain(|d| !drained_names.contains(d))
        }

        ordered_pkgs.extend(drained);
    }

    println!("{ordered_pkgs:?}");

    if !tmp_pkgs.is_empty() {
        println!("Cycled:");
        println!("{tmp_pkgs:?}");
    }

    // Step 4 - generate packages

    let package_root = Path::new("/tmp/bloom"); //tempdir()?;

    // Generate a rosdep yaml file

    let mut rosdistro = String::new();

    for (p, pkg, _, _, _) in &ordered_pkgs {
        writeln!(rosdistro, "{p}:\n  ubuntu: [{pkg}]",)?;
    }

    let mut rosdep = File::create(package_root.join("package.yaml"))?;
    rosdep.write_all(rosdistro.as_bytes())?;

    // Generate rosdep list file
    let mut rosdep = File::create("/etc/ros/rosdep/sources.list.d/99-cargo-bloom.list")?;
    writeln!(
        rosdep,
        "yaml file://{}/package.yaml",
        package_root.canonicalize()?.display()
    )?;

    // Generate a debian list file
    let mut deb = File::create("/etc/apt/sources.list.d/99-cargo-bloom.list")?;
    writeln!(
        deb,
        "deb [trusted=yes] file://{} /",
        package_root.canonicalize()?.display()
    )?;

    //let _ = OpenOptions::new()
    //    .create(true)
    //    .truncate(true)
    //    .write(true)
    //    .append(false)
    //    .open(package_root.join("Packages"))?;

    // Update rosdep

    Command::new("rosdep").arg("update").output()?;

    // Build packages one by one

    for (p, _, d, _, i) in ordered_pkgs {
        if !checked_pkgs.contains(&p.as_str())
            && (pkgs_to_check.is_empty() || pkgs_to_check.contains(&p.as_str()))
        {
            println!("{i} {package_root:?}");

            bloom(&p, package_root, &d).map_err(|e| {
                println!("ERROR: {e}");

                let mut s = vec![];
                std::io::stdin().lock().read(&mut s).unwrap();
                e
            })?;
        }
    }

    let mut s = vec![];
    std::io::stdin().lock().read(&mut s).unwrap();

    Ok(())
}
