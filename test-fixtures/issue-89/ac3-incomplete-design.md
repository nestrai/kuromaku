# Design: Add --verbose flag to CLI

## Requirement
Users should be able to enable verbose output.

## Design
Add a `verbose: bool` field to the CLI args struct. When enabled, print more information.

This design is incomplete because it does not specify:
- What specific information should verbose mode print?
- Where in the codebase should verbose logging be added?
- Should verbose mode affect all commands or only some?
- How should verbose output be formatted (stdout, stderr, structured logging)?
