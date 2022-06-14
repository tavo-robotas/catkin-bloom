# catkin-bloom

## Export entire catkin workspace into a native packages

catkin-bloom is an utility program meant to help building ROS packages locally, without resorting to deploying the complicated ROS build farm.

catkin-bloom primarily aims to support running under ROS docker images, thus debian packaging is currently supported. In addition, this is a very early version that only supports ROS melodic on Ubuntu bionic. More flexibility is under way.

### How to run

First, install rust toolchain, and install catkin-bloom through it

```
cargo install --path .
```

Or, when this package is on crates.io:

```
cargo install catkin-bloom
```

Then, run the program on the workspace

```
catkin-bloom
```

Lots of things TODO, but the resulting debs will be found under /tmp/bloom directory.

In addition, this program will install all of those packages, to cleanup, run the following:

```
apt remove $(cd /tmp/bloom; for p in *.deb; do echo $p | cut -f1 -d"_"; done)
```

### Explanation

The way catkin-bloom works is by walking the entire workspace, parsing dependencies, and sorting packages in a way that all dependencies are built before the dependents. Cycles are assumed to not exist (since they are illegal anyways). The packages are then ordered in tiers, where all packages in a single tier are completely independent (and may only depend on the lower tiers). See below figure:

```text
    T 1    |    T 2    |    T 3    |    T 4
-----------+-----------+-----------+-----------

   +---+       +---+                   +---+
   | A |------>| D |---------------+-->| H |
   +---+       +---+               |   +---+
     |                             |
     +-----+-----------------------+
           |                       |
   +---+   |   +---+       +---+   |
   | B |---+-->| E |------>| F |   |
   +---+       +---+       +---+   |
     |           |                 |
     +-----------+-----+           |
                       |           |
   +---+               |   +---+   |
   | C |---------------+-->| G |---+
   +---+                   +---+

```

Not done yet, but packages A, B, C can be built concurrently, speeding up the process. Same with D, E, and F, G respectively.