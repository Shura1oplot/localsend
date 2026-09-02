# Project overview

## Goal

Create a minimal macOS command-line program and a thin Dropzone 5 integration from the existing LocalSend code. The program sends either one file or one text message to the LocalSend device named `RUGM0Q3RJ0` on the local network.

## User and usage

The user runs the program from scripts and interactive terminal sessions on macOS. The destination is fixed, so routine use must not require selecting a device or passing its name.

## Success criteria

1. A single executable accepts either a file path or text.
2. It resolves `RUGM0Q3RJ0.local` and accepts only the pinned HTTPS identity of `RUGM0Q3RJ0`.
3. It uses the LocalSend protocol and reuses the repository's Rust core.
4. It has no Flutter or other GUI runtime dependency.
5. It reports errors with enough detail to correct them and returns a nonzero exit status on failure.
6. Its automated tests cover argument validation, destination selection, and transfer orchestration without requiring the destination to be online.
7. It builds and passes formatting, linting, and tests on macOS.
8. The Dropzone 5 integration accepts dragged files or text, sends clipboard images or text when clicked, and reports the command-line program's result without adding a GUI dependency to the Rust package.

## Scope

Included:

- LocalSend protocol v2 transfer of one file or one text message.
- Direct mDNS resolution of the fixed destination.
- A concise usage document and macOS build instructions.
- A minimal Dropzone 5 integration that invokes the command-line sender.

Excluded:

- Receiving files.
- Interactive device selection.
- Transfer history, settings UI, tray integration beyond the Dropzone integration, and automatic updates.
- Compatibility wrappers for the Flutter application.

## Constraints and decisions

- Prefer the existing `packages/core` Rust crate and the existing `cli` package.
- Use small local adapters or stubs where GUI integration would otherwise be required.
- Keep changes isolated from the Flutter application to reduce conflicts with upstream `main`.
- Do not modify or discard unrelated working-tree changes.

## Current state

The `cli` package builds a macOS arm64 command-line sender. It reuses the Rust LocalSend HTTP core, resolves the fixed target through mDNS, and pins the target's preconfigured HTTPS certificate. A temporary callback server preserves receiver-initiated cancellation without depending on reverse discovery. Automated tests cover argument handling, target identity, sender identity persistence, receiver cancellation, text and file response semantics, server cleanup, and a local end-to-end HTTPS transfer. The Dropzone 5 integration is installed in the main grid and correctly invokes the sender. A live Dropzone transfer of `cli/Cargo.toml` to `RUGM0Q3RJ0` completed successfully. Clicking the action also sends a PNG image from the clipboard through a temporary file that is removed after the transfer.
