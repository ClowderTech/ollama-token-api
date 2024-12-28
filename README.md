# Ollama Token API

The **ollama-token-api** project aims to add bearer authentication to Ollama, allowing users to secure their access without needing different client libraries. It works by validating tokens stored in a MongoDB database and reverse proxying requests to the Ollama server using Hono.

## Table of Contents
- [Features](#features)
- [Installation](#installation)
  - [Using Docker (Recommended)](#using-docker-recommended)
  - [Manual Installation](#manual-installation)
- [Usage](#usage)
- [Contributing](#contributing)
- [License](#license)

## Features

- **Bearer Authorization:** Implement OAuth2 bearer token authentication for Ollama.
- **MongoDB Token Storage:** Store and retrieve tokens from a MongoDB database.
- **Honol Reverse Proxying:** Use Hono to reverse proxy requests to the Ollama server.
- **Highly Customizable:** Configurable via environmental variables or configuration files.

## Installation

### Using Docker (Recommended)

You can deploy this API using Docker. The official Docker image is available on both GitHub and our private registry.

#### Pulling Docker Image from Public Registry
```sh
docker pull ghcr.io/clowdertech/ollama-token-api:latest
```

Or, for a specific tag:
```sh
docker pull ghcr.io/clowdertech/ollama-token-api:<tag>
```

#### Pulling Docker Image from Private Forgejo Registry

If you aim to contribute or access the latest development builds, use our dedicated registry.

```sh
docker pull forgejo.clowdertech.com/clowdertech/ollama-token-api:master
```

### Manual Installation

TODO: Add detailed installation instructions for manual setup (including setting up MongoDB and Hono).

## Usage

Once installed, start using Ollama Token API by sending requests with appropriately authenticated bearer tokens.

For additional configuration options or advanced usage, please refer to the following file:
- [docs/configuration.md](docs/configuration.md)

## Contributing
Contributions are highly welcomed! If you wish to contribute to this project, please follow these steps:

1. Fork this repository using Forgejo (https://forgejo.clowdertech.com/clowdertech/ollama-token-api).
2. Create a new feature branch from `master`.
3. Commit your changes following standard conventions.
4. Push the branch to your fork and create a Pull Request.

For more details on contribution guidelines, see:
- [CONTRIBUTING.md](CONTRIBUTING.md)

## License
This project is licensed under the terms of the **Apache License**. See LICENSE file for full details.

---
*Built with ❤️ by ClowderTech*