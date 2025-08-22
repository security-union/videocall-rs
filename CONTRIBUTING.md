# Contributing to videocall.rs

First off, thank you for considering contributing to videocall.rs! It's people like you that make this project such a great tool. This document provides guidelines and steps for contributing.

## Code of Conduct

By participating in this project, you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md). Please read it before contributing.

## How Can I Contribute?

### Reporting Bugs

This section guides you through submitting a bug report. Following these guidelines helps maintainers understand your report, reproduce the behavior, and find related reports.

#### Before Submitting a Bug Report

* Check the [GitHub issues](https://github.com/security-union/videocall-rs/issues) to see if the problem has already been reported. If it has and the issue is still open, add a comment to the existing issue instead of opening a new one.
* Collect information about the bug:
  * Stack trace (if applicable)
  * OS and version
  * Browser version (if applicable)
  * Steps to reproduce the issue
  * Expected behavior
  * Actual behavior

#### Submitting a Bug Report

Bugs are tracked as GitHub issues. Create an issue and provide the following information:

* Use a clear and descriptive title
* Describe the exact steps to reproduce the bug
* Provide specific examples to demonstrate the steps
* Describe the behavior you observed after following the steps
* Explain which behavior you expected to see instead and why
* Include screenshots or animated GIFs if possible

### Suggesting Enhancements

This section guides you through submitting an enhancement suggestion, including completely new features and minor improvements to existing functionality.

#### Before Submitting an Enhancement Suggestion

* Check the [GitHub issues](https://github.com/security-union/videocall-rs/issues) to see if the enhancement has already been suggested.
* Determine which repository the enhancement should be suggested in (is it related to the backend, frontend, or a specific component?).

#### Submitting an Enhancement Suggestion

Enhancement suggestions are tracked as GitHub issues. Create an issue and provide the following information:

* Use a clear and descriptive title
* Provide a detailed description of the suggested enhancement
* Explain why this enhancement would be useful to most users
* Include any relevant mockups or diagrams
* List specific examples of how this enhancement would be used

### Pull Requests

#### Setting Up Development Environment

1. Fork the repository
2. Clone your fork locally:
   ```
   git clone https://github.com/your-username/videocall-rs.git
   cd videocall-rs
   ```
3. Add the original repository as a remote:
   ```
   git remote add upstream https://github.com/security-union/videocall-rs.git
   ```
4. Follow the setup instructions in the README.md to configure your development environment.

#### Creating a Pull Request

1. Create a new branch from the latest `main`:
   ```
   git checkout main
   git pull upstream main
   git checkout -b feature/your-feature-name
   ```
2. Make your changes
3. Follow the coding standards and run tests:
   ```
   cargo fmt
   cargo clippy -- -D warnings
   cargo tests_run
   ```
4. Commit your changes with a descriptive commit message following the [Conventional Commits](https://www.conventionalcommits.org/) specification:
   ```
   git commit -m "feat: add new feature"
   ```
5. Push to your fork:
   ```
   git push origin feature/your-feature-name
   ```
6. Create a pull request from your fork to the main repository

#### Pull Request Guidelines

* Update documentation for significant changes
* Add tests for new functionality
* Maintain the existing code style
* Keep pull requests focused on a single concern
* Link related issues in the pull request description
* Be prepared to address feedback and make changes if requested

## RFC Process

For significant changes that require broader discussion, we use a Request for Comments (RFC) process:

1. Check the [rfc directory](/rfc) for existing RFCs related to your proposal
2. Create a new markdown file in the RFC directory following the naming convention: `rfc-XX-your-proposal-name.md`
3. Use the template format from existing RFCs
4. Submit a PR with your RFC
5. The RFC will be discussed with the community and core team
6. Once approved, the implementation can begin

## Development Guidelines

### Coding Standards

* Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
* Use meaningful variable and function names
* Keep functions focused on a single responsibility
* Comment complex logic or non-obvious decisions
* Use `cargo fmt` and `cargo clippy` before committing

### Testing

* Write unit tests for new functionality
* Ensure all tests pass before submitting a PR
* Include integration tests for API changes
* Test browser compatibility for frontend changes

### Documentation

* Update API documentation for public interfaces
* Add JSDoc or equivalent comments for JavaScript/TypeScript
* Update README and other docs for user-facing changes
* Document new features with examples

## Community

Join our [Discord server](https://discord.gg/JP38NRe4CJ) to discuss development, ask questions, and get help.

## Recognition

Contributors will be acknowledged in the project README.

Thank you for your contributions! 