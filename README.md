# m2c

`m2c` (**M**akefile **to** **c**ompile_commands) parses NMAKE-style Makefiles
(as found in EDK2/UEFI-style build systems) and generates a
[`compile_commands.json`](https://clang.llvm.org/docs/JSONCompilationDatabase.html)
compilation database. That database can then be consumed by `clangd` or
Microsoft's C/C++ extension to provide accurate IntelliSense (code completion,
go-to-definition, diagnostics, etc.) for codebases that don't build with
CMake or another compilation-database-aware build system.

## Building

```sh
cargo build --release
```

The binary will be available at `target/release/m2c`.

## Usage

```sh
m2c <path/to/Makefile> [compile_commands.json] [OPTIONS]
```

Options:

| Flag | Description |
| --- | --- |
| `-s`, `--stdin` | Read the Makefile path from stdin instead of the positional argument. |
| `-w`, `--walk-dir <DIR>` | Recursively walk `DIR` and process every `Makefile`/`GNUmakefile` found. |
| `-a`, `--append` | Merge newly discovered compile units into the existing `compile_commands.json` instead of overwriting it. |
| `--append-from <FILE>` | Merge into `FILE` instead of the output path. |
| `--warn-dup` | Warn when a file already has an entry in the compilation database. |
| `-P`, `--no-pretty` | Write compact JSON instead of pretty-printed JSON. |
| `-v`, `--verbose` | Enable debug logging. |

The most common use case is walking an EDK2-style `Build` output directory
(where the per-module Makefiles live after a build) and writing
`compile_commands.json` at the repo root:

```sh
m2c --walk-dir Build
```

Re-run the command (with `--append`) whenever the Makefiles change to keep
the compilation database up to date.

## Configuring clangd to use compile_commands.json

`clangd` automatically discovers a `compile_commands.json` if it is located
in the project root, or in a `build/` directory at the root. If you generate
the file somewhere else, point `clangd` at it explicitly with a
`.clangd` config file at the repo root:

```yaml
CompileFlags:
  CompilationDatabase: .   # directory containing compile_commands.json
```

Or pass it on the command line:

```sh
clangd --compile-commands-dir=/path/to/dir/containing/json
```

## VS Code Microsoft C/C++ extension (cpptools)

Open your `settings.json` (`Ctrl+Shift+P` →
`Preferences: Open User Settings (JSON)`) and set `C_Cpp.default.compileCommands`.
cpptools substitutes `${workspaceFolder}` per-workspace, so this applies
automatically to any repo you open that has a `compile_commands.json` at its
root:

```json
{
  "C_Cpp.default.compileCommands": "${workspaceFolder}/compile_commands.json"
}
```

If the file doesn't exist for a given workspace, cpptools silently falls
back to its normal `includePath`/`defines`-based IntelliSense — so it's safe
to leave this set globally. Whenever `m2c --walk-dir Build` (re)generates the
file at the workspace root, just run **C/C++: Reset IntelliSense Database**
(or reload the window) to pick up the changes.


### VS Code clangd extension

1. Install the [`clangd` extension](https://marketplace.visualstudio.com/items?itemName=llvm-vs-code-extensions.vscode-clangd) (`llvm-vs-code-extensions.vscode-clangd`).
2. Disable Microsoft's C/C++ IntelliSense engine to avoid conflicts, either by
   uninstalling the `ms-vscode.cpptools` extension or by adding this to
   `.vscode/settings.json`:

   ```json
   {
     "C_Cpp.intelliSenseEngine": "disabled",
     "clangd.path": "clangd",
     "clangd.arguments": [
       "--compile-commands-dir=${workspaceFolder}"
     ]
   }
   ```

3. Run `m2c` to (re)generate `compile_commands.json` at the workspace root
   (or wherever `--compile-commands-dir` points).
4. Reload the window / restart the clangd language server
   (`clangd: Restart language server` from the command palette) so it picks
   up the compilation database.

## Notes

- `compile_commands.json` should be regenerated (with `m2c --append`) any
  time Makefiles change, since it is a point-in-time snapshot of the build.
- Both `clangd` and `cpptools` only need `compile_commands.json` to be
  discoverable — they do not need the underlying `make`/`nmake` build to
  succeed for IntelliSense to work.
