# First stage: Build the Rust application
FROM rust:1-slim-bullseye
RUN apt-get update && apt-get install -y pkg-config libssl-dev libpq-dev curl

# Install Node.js
RUN curl -fsSL https://deb.nodesource.com/setup_current.x | bash -
RUN apt install -y nodejs

# Install Java
RUN apt install -y default-jdk

# Install OpenAPI Generator CLI globally
RUN npm install @openapitools/openapi-generator-cli -g

RUN bash -c "$(curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/rust-helpers/install-pipeline-clis.sh)"

ARG GINGER_TOKEN
ENV GINGER_TOKEN=$GINGER_TOKEN
RUN ginger-auth token-login $GINGER_TOKEN

WORKDIR /app

# Copy the current directory contents into the container at /app
COPY . .

RUN ginger-connector connect stage
RUN cargo build