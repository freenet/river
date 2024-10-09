# Project README

![Screenshot of chat interface](screenshot-20241009.png)

## Project Structure

This repository contains several crates, each serving a specific purpose in our project:

### Core Crate
- **Name**: `core`
- **Purpose**: Provides the fundamental data structures and algorithms used throughout the project.
- **Key Features**: 
  - Implementation of the DHT (Distributed Hash Table)
  - Basic networking primitives

### Network Crate
- **Name**: `network`
- **Purpose**: Handles all network-related functionality, building on top of the core crate.
- **Key Features**:
  - Peer discovery
  - Message routing
  - Connection management

### Storage Crate
- **Name**: `storage`
- **Purpose**: Manages data persistence and retrieval.
- **Key Features**:
  - Local data storage
  - Caching mechanisms
  - Data replication strategies

### CLI Crate
- **Name**: `cli`
- **Purpose**: Provides a command-line interface for interacting with the network.
- **Key Features**:
  - User-friendly commands for network operations
  - Debugging and monitoring tools

### GUI Crate (Optional)
- **Name**: `gui`
- **Purpose**: Offers a graphical user interface for those who prefer visual interaction.
- **Key Features**:
  - Network visualization
  - Easy-to-use controls for common operations

## Getting Started

(Add instructions on how to build and run the project)

## Contributing

(Add guidelines for contributing to the project)

## License

(Specify the license under which this project is released)
