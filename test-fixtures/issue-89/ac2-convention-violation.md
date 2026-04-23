# Design: Error Handling for Config Parser

## Requirement
Parse YAML config file, return error if invalid.

## Design
Create a `parse_config(path: &str)` function that:
- Reads the file
- Parses YAML
- Returns `null` if parsing fails
- Returns the parsed config struct on success

This violates the rust-developer rule which states "All Results must be handled -- no silent unwraps" and "functions cannot return null in Rust, must use Result<T, E>".
