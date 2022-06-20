use anyhow::{anyhow, Result};
use clap::*;
use log::*;
use quick_xml::{events::Event, Reader};
use rayon::{iter::*, *};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env::current_dir;
use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::tempdir;
use walkdir::WalkDir;

fn main() -> Result<()> {
    env_logger::init();

    let args = parse_args();

    let args = (&args).into();

    debug!("{args:?}");

    let RuntimeArgs {
        os_name,
        os_version,
        ros_distro,
        repo_path,
        ignored_pkgs,
        only_check,
        src,
        jobs,
        extra_repos,
        noinstall_deps,
        rosdep_defs,
    } = args;

    let pool = ThreadPoolBuilder::new().num_threads(jobs).build().unwrap();

    let mut pkgs = HashMap::new();
    let mut workspace_pkgs = HashSet::new();

    // Step 1 - collect all dependencies in the workspace
    println!("Collecting packages");

    for entry in WalkDir::new(src) {
        if let Ok(entry) = entry {
            if entry.file_type().is_file() && entry.file_name() == OsStr::new("package.xml") {
                debug!("Found {}", entry.path().display());

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

    trace!("{pkgs:?}");

    // Step 3 - sort the packages in the dependency fullfilling order
    let mut ordered_pkgs = vec![];

    let mut tmp_pkgs = pkgs
        .iter()
        .map(|(n, (p, d))| (n.clone(), p.clone(), d.clone()))
        .collect::<Vec<_>>();

    for _ in 0.. {
        let mut drained = vec![];
        let mut drained_names = HashSet::new();

        let mut i = 0;
        while i < tmp_pkgs.len() {
            if tmp_pkgs[i].2.is_empty() {
                trace!("REMOVE {}", tmp_pkgs[i].0);
                let (name, path, deps) = tmp_pkgs.swap_remove(i);
                drained_names.insert(name.clone());
                let pkg = format!("ros-{ros_distro}-{}", name.replace("_", "-"));
                drained.push((name, pkg, path, deps));
            } else {
                i += 1;
            }
        }

        if drained.is_empty() {
            break;
        }

        trace!("DRAIN {:?}", drained);

        for (_, _, d) in &mut tmp_pkgs {
            d.retain(|d| !drained_names.contains(d))
        }

        ordered_pkgs.push(drained);
    }

    trace!("{ordered_pkgs:?}");

    if !tmp_pkgs.is_empty() {
        warn!("Found packages with cycles: {tmp_pkgs:?}");
    }

    // Step 4 - generate packages

    let package_root = Path::new(repo_path);
    fs::create_dir_all(&package_root)?;

    // Generate a rosdep yaml file

    let mut rosdistro = String::new();

    for (p, pkg, _, _) in ordered_pkgs.iter().flatten() {
        writeln!(rosdistro, "{p}:\n  {os_name}: [{pkg}]",)?;
    }

    for (k, v) in rosdep_defs {
        writeln!(rosdistro, "{k}:\n  {os_name}: [{v}]",)?;
    }

    let mut rosdep = File::create(package_root.join("package.yaml"))?;
    rosdep.write_all(rosdistro.as_bytes())?;

    for (i, path) in std::iter::once(repo_path)
        .chain(extra_repos.iter().copied())
        .enumerate()
    {
        let package_root = Path::new(path);

        let repo_path_name = package_root
            .file_name()
            .and_then(|p| p.to_str())
            .unwrap_or("unknown");

        // Generate rosdep list file
        let mut rosdep = File::create(&format!(
            "/etc/ros/rosdep/sources.list.d/99-catkin-bloom-{i}-{repo_path_name}.list"
        ))?;
        writeln!(
            rosdep,
            "yaml file://{}/package.yaml",
            package_root.canonicalize()?.display()
        )?;

        // Generate a debian list file
        let mut deb = File::create(&format!(
            "/etc/apt/sources.list.d/99-catkin-bloom-{i}-{repo_path_name}.list"
        ))?;
        writeln!(
            deb,
            "deb [trusted=yes] file://{} /",
            package_root.canonicalize()?.display()
        )?;
    }

    // Update rosdep

    println!("Run rosdep update");

    Command::new("rosdep").arg("update").output()?;

    // Install dependencies if enabled

    if !noinstall_deps {
        println!("Installing dependencies");

        info!("Run rosdep check");

        // First install all apt dependencies in an optimized way
        let o = Command::new("rosdep")
            .args(["check", "--from-paths", src, "--ignore-src"])
            .output()?;

        info!("Run apt update");

        Command::new("apt").arg("update").output()?;

        info!("Run apt install");

        let o = Command::new("apt")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .args(["install", "-y"])
            .args(
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| l.strip_prefix("apt\t"))
                    .map(str::trim),
            )
            .output()?;

        if o.status.code().unwrap_or_default() != 0 {
            return Err(anyhow!(
                "Failed to do apt install '{}' | '{}'",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr),
            ));
        }

        // Then install all other dependencies
        info!("Run rosdep install");

        let o = Command::new("rosdep")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .args(["install", "--from-paths", src, "--ignore-src", "-y"])
            .output()?;

        if o.status.code().unwrap_or_default() != 0 {
            return Err(anyhow!(
                "Failed to do rosdep install '{}' | '{}'",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ));
        }
    }

    // Build packages one by one
    let pkg_count = ordered_pkgs.iter().flatten().count();
    println!("Building packages ({pkg_count})");

    let pb = indicatif::ProgressBar::new(pkg_count as u64);
    pb.enable_steady_tick(100);

    for (i, pkgs) in ordered_pkgs.iter().enumerate() {
        pb.println(&format!("Layer {i}"));

        pool.install(|| {
            let success = AtomicBool::new(true);

            let debs = pkgs
                .par_iter()
                .flat_map(|(p, _, d, _)| {
                    if success.load(Ordering::Relaxed)
                        && only_check
                            .as_ref()
                            .map(|v| v.contains(&p.as_str()))
                            .unwrap_or(true)
                    {
                        match bloom(&p, package_root, &d, os_name, os_version, ros_distro) {
                            Err(e) => {
                                error!("{p}: {e}");
                                success.store(false, Ordering::Relaxed);
                                vec![]
                            }
                            Ok(debs) => debs,
                        }
                    } else {
                        vec![]
                    }
                })
                .inspect(|_| pb.inc(1))
                .collect::<Vec<_>>();

            if success.load(Ordering::Relaxed) {
                let o = Command::new("dpkg").args(["-i"]).args(debs).output()?;

                trace!(
                    "stdout:\n{}\n\nstderr:\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );

                Ok(())
            } else {
                Err(anyhow!("Error building one of the packages"))
            }
        })?;
    }

    pb.finish();

    println!("Generating Package manifest");

    let mut packages = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .append(false)
        .open(package_root.join("Packages"))?;

    let o = Command::new("dpkg-scanpackages")
        .args(["-m", "."])
        .current_dir(&package_root)
        .output()?;

    packages.write(&o.stdout)?;

    Ok(())
}

fn parse_args() -> ArgMatches {
    clap::Command::new("catkin-bloom")
        .version(crate_version!())
        .author(crate_authors!())
        .arg(
            Arg::new("os-name")
                .long("os-name")
                .takes_value(true)
                .default_value("ubuntu"),
        )
        .arg(
            Arg::new("os-version")
                .long("os-version")
                .takes_value(true)
                .default_value("bionic"),
        )
        .arg(
            Arg::new("ros-distro")
                .long("ros-distro")
                .takes_value(true)
                .default_value("melodic"),
        )
        .arg(
            Arg::new("ignore-pkgs")
                .long("ignore-pkgs")
                .takes_value(true)
                .multiple_values(true)
                .use_value_delimiter(true),
        )
        .arg(
            Arg::new("only-check")
                .long("only-check")
                .takes_value(true)
                .multiple_values(true)
                .use_value_delimiter(true),
        )
        .arg(
            Arg::new("repo-path")
                .long("repo-path")
                .short('r')
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::new("noinstall-deps")
                .long("noinstall-deps")
                .short('n')
                .takes_value(false),
        )
        .arg(
            Arg::new("rosdep-defs")
                .long("rosdep-defs")
                .short('D')
                .takes_value(true)
                .multiple_values(true)
                .use_value_delimiter(true),
        )
        .arg(
            Arg::new("extra-repos")
                .long("extra-repos")
                .short('e')
                .takes_value(true)
                .multiple_values(true)
                .use_value_delimiter(true),
        )
        .arg(Arg::new("jobs").long("jobs").short('j').takes_value(true))
        .arg(Arg::new("src").takes_value(true).default_value("."))
        .get_matches()
}

#[derive(Debug)]
struct RuntimeArgs<'a> {
    os_name: &'a str,
    os_version: &'a str,
    ros_distro: &'a str,
    repo_path: &'a str,
    ignored_pkgs: Vec<&'a str>,
    extra_repos: Vec<&'a str>,
    only_check: Option<Vec<&'a str>>,
    rosdep_defs: Vec<(&'a str, &'a str)>,
    src: &'a str,
    jobs: usize,
    noinstall_deps: bool,
}

impl<'a> From<&'a ArgMatches> for RuntimeArgs<'a> {
    fn from(matches: &'a ArgMatches) -> Self {
        Self {
            os_name: matches.value_of("os-name").unwrap(),
            os_version: matches.value_of("os-version").unwrap(),
            ros_distro: matches.value_of("ros-distro").unwrap(),
            repo_path: matches.value_of("repo-path").unwrap(),
            ignored_pkgs: matches
                .values_of("ignore-pkgs")
                .into_iter()
                .flatten()
                .collect(),
            extra_repos: matches
                .values_of("extra-repos")
                .into_iter()
                .flatten()
                .collect(),
            rosdep_defs: matches
                .values_of("rosdep-defs")
                .into_iter()
                .flatten()
                .filter_map(|l| l.split_once("="))
                .collect(),
            only_check: matches.values_of("only-check").map(Iterator::collect),
            src: matches.value_of("src").unwrap(),
            jobs: matches
                .value_of("jobs")
                .and_then(|j| usize::from_str_radix(j, 10).ok())
                .unwrap_or(1),
            noinstall_deps: matches.occurrences_of("noinstall_deps") > 0,
        }
    }
}

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

fn bloom(
    pkg: &str,
    package_dir: &Path,
    path: &Path,
    os_name: &str,
    os_version: &str,
    ros_distro: &str,
) -> Result<Vec<PathBuf>> {
    let build_root = tempdir()?;

    let pb = build_root.path().join("build");
    fs::create_dir(&pb)?;

    let cwd = current_dir()?;
    let p = cwd.join(path);

    // Generate debian build directory

    let o = Command::new("bloom-generate")
        .args([
            "rosdebian",
            "--os-name",
            os_name,
            "--os-version",
            os_version,
            "--ros-distro",
            ros_distro,
        ])
        .arg(&p)
        .current_dir(&pb)
        .output()?;

    if o.status.code().unwrap_or_default() != 0 {
        error!(
            "stdout:\n{}\n\nstderr:\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );

        return Err(anyhow!("bloom-generate failed!"));
    }

    // Patch debian/rules to use the correct package path

    let rules_path = pb.join("debian/rules");
    let rules = fs::read_to_string(&rules_path)?.replace(
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

    if o.status.code().unwrap_or_default() != 0 {
        error!(
            "stdout:\n{}\n\nstderr:\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        return Err(anyhow!("Failed to do {pkg}"));
    }

    // Copy the generated debs out and update the package list

    let o = Command::new("dpkg-scanpackages")
        .args(["-m", "."])
        .current_dir(&build_root)
        .output()?;

    let mut debs = vec![];

    for fp in String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(str::trim)
        .filter_map(|s| s.strip_prefix("Filename: "))
        .map(Path::new)
    {
        debug!("{}", fp.display());
        let origin = build_root.path().join(fp);
        let target = package_dir.join(fp);
        debug!("Copied to: {}", target.display());
        fs::copy(&origin, &target)?;
        debs.push(target);
    }

    Ok(debs)
}
