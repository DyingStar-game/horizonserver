FROM rust:1.89.0-bookworm

ARG USERNAME=rust
ARG USER_UID=1000
ARG USER_GID=$USER_UID

# System dependencies
RUN apt-get update && apt-get install -y --no-install-recommends sudo git protobuf-compiler libprotobuf-dev
RUN rm -rf /var/lib/apt/lists/*

# Ensure user/group exist with expected UID/GID (works whether rust user exists or not)
RUN set -eux; \
  if getent group "${USERNAME}" >/dev/null; then \
    groupmod -g "${USER_GID}" "${USERNAME}" || true; \
  else \
    groupadd -g "${USER_GID}" "${USERNAME}"; \
  fi; \
  if id -u "${USERNAME}" >/dev/null 2>&1; then \
    usermod -u "${USER_UID}" -g "${USER_GID}" "${USERNAME}" || true; \
  else \
    useradd -m -u "${USER_UID}" -g "${USER_GID}" -s /bin/bash "${USERNAME}"; \
  fi; \
  echo "${USERNAME} ALL=(root) NOPASSWD:ALL" > "/etc/sudoers.d/${USERNAME}"; \
  chmod 0440 "/etc/sudoers.d/${USERNAME}"

RUN mkdir -p /app
RUN chown -R $USERNAME:$USERNAME /app
RUN chmod 775 /app


USER $USERNAME
WORKDIR /app

## used for build and dev on rust

RUN cargo install --git https://github.com/Far-Beyond-Dev/FB-CLI


CMD [ "tail","-f","/dev/null" ]
