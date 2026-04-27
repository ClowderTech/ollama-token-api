ARG DENO_VERSION=2.7.13
ARG UID=10001

# Use the latest Node.js image.
FROM denoland/deno:debian-${DENO_VERSION}
ARG UID

ENV DEBIAN_FRONTEND=noninteractive

# Set the working directory inside the Docker container.
WORKDIR /app

RUN groupadd \
    --gid "${UID}" \
    --system \
    appuser \ 
    && useradd \
    --create-home \
    --uid "${UID}" \
    --gid "${UID}" \
    --no-log-init \
    --system \
    appuser

# Copy package.json to Docker image.
COPY deno.json ./

# Install Npm dependencies.
RUN deno install

# Run everything after as non-privileged user.
USER appuser

# Copy all other files from the current directory to /app in the container.
COPY . .

EXPOSE 8000

# Command to run the application.
CMD ["deno", "task", "start"]
