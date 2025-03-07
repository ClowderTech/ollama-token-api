# Use the latest Node.js image.
FROM node:current-slim

# Set the working directory inside the Docker container.
WORKDIR /app

# Copy package.json to Docker image.
COPY package.json ./

# Install Npm dependencies.
RUN npm install 

# Add user so we don't need --no-sandbox.
RUN groupadd clowdertech && useradd -g clowdertech clowdertech \
    && mkdir -p /home/clowdertech/Downloads /app \
    && chown -R clowdertech:clowdertech /home/clowdertech \
    && chown -R clowdertech:clowdertech /app

# Run everything after as non-privileged user.
USER clowdertech

# Copy all other files from the current directory to /app in the container.
COPY . .

EXPOSE 8000

# Command to run the application.
CMD ["npm", "run", "start"]