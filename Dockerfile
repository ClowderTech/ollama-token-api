# Use the latest Deno image
FROM denoland/deno:debian

# Set the working directory inside the Docker container
WORKDIR /app

# Copy deno package files to Docker image
COPY deno.json ./

# Install Deno dependencies
RUN deno install

# Copy all other files from the current directory to /app in the container
COPY . .

# Expose container port
EXPOSE 8000

# Command to run the application
CMD ["deno", "run", "prodstart"]