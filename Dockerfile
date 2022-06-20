ARG ROS_TAG=melodic
FROM ros:${ROS_TAG}

ENV RUSTUP_HOME=/rust
ENV CARGO_HOME=/cargo 

RUN apt update && \
	DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
		curl dh-make python-bloom fakeroot && \
	apt clean && \
    rm -rf /var/lib/apt/lists/*

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable --no-modify-path

RUN mkdir /catkin-bloom

COPY Cargo.toml Cargo.lock /catkin-bloom/
COPY src/ /catkin-bloom/src/

RUN cd /catkin-bloom \
	&& PATH=/cargo/bin:/rust/bin:$PATH cargo install --path . \
	&& cd / \
	&& rm -rf /catkin-bloom \
	&& cp /cargo/bin/catkin-bloom /usr/local/bin \
	&& rm -rf /rust /cargo

RUN echo '#!/bin/bash \n\
set -e \n\
\n\
# setup ros environment \n\
args="$@" \n\
set -- \n\
source "/opt/ros/$ROS_DISTRO/setup.bash" \n\
exec $args' > /ros_entrypoint.sh

RUN echo '#!/bin/bash \n\
\n\
/ros_entrypoint.sh catkin-bloom \\\n\
	--os-name $(lsb_release -is | tr '[:upper:]' '[:lower:]') \\\n\
	--os-version $(lsb_release -cs) \\\n\
	--ros-distro $ROS_DISTRO \\\n\
	"$@"' >> /catkin_bloom_entry.sh \
	&& chmod 755 /catkin_bloom_entry.sh

ENTRYPOINT ["/catkin_bloom_entry.sh"]

CMD ["--repo-path", "/repo", "-j8", "/workspace"]
