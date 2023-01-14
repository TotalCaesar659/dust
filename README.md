# Dust

![Screenshot](screenshot.png)

[![Build and test status](https://github.com/Kelpsy/dust/actions/workflows/.github/workflows/run-clippy-and-test.yml/badge.svg?branch=main&event=push)](https://github.com/Kelpsy/dust/actions/workflows/run-clippy-and-test.yml?query=branch%3Amain+event%3Apush)

## Web version

[![Web deploy status](https://github.com/Kelpsy/dust/actions/workflows/.github/workflows/deploy-web.yml/badge.svg?branch=main&event=push)](https://github.com/Kelpsy/dust/actions/workflows/deploy-web.yml?query=branch%3Amain+event%3Apush)

[Web frontend](https://dust-emu.netlify.app)

## Prebuilt native binaries

There are three versions available for each operating system:
- Release: only includes details useful to an end user
- Debug: includes logging and debugging views that could be useful for game developers or to do some simple analysis on game behavior
- Debug + GDB server: includes everything in the debug version, plus a stub to connect a GDB client enabled for 32-bit ARM targets directly to the emulator, enabling more advanced interaction and debugging of the emulated CPU

Do note that *none* of these versions include debug symbols for the emulator itself; they are all compiled with the same set of optimizations and LTO, and aren't suitable for debugging of the emulator; for that purpose, the only solution at the moment is a local build using the `release` profile must be used (`debug` profile builds are currently unstable due to an upstream Rust issue); accordingly, debug symbols are already enabled in `Cargo.toml`.

[![Release build status](https://github.com/Kelpsy/dust/actions/workflows/.github/workflows/build-release.yml/badge.svg?branch=main&event=push)](https://github.com/Kelpsy/dust/actions/workflows/build-release.yml?query=branch%3Amain+event%3Apush)

| Release | Debug | Debug + GDB server |
| ------- | -------------------------------------------- | ------------------ |
| [Windows (release)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/Windows.zip) | [Windows (debug)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/Windows-debug.zip) | [Windows (debug + GDB)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/Windows-debug-gdb.zip) |
| [Linux (release)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/Linux.zip) | [Linux (debug)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/Linux-debug.zip) | [Linux (debug + GDB)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/Linux-debug-gdb.zip) |
| [macOS (release)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/macOS.zip) | [macOS (debug)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/macOS-debug.zip) | [macOS (debug + GDB)](https://nightly.link/Kelpsy/dust/workflows/build-release/main/macOS-debug-gdb.zip) |

# Credits
- Martin Korth, for summarizing resources on the DS on [GBATEK](https://problemkaputt.de/gbatek.htm)
- Arisotura, for her research on the system in melonDS, [test ROMs](https://github.com/Arisotura/arm7wrestler) and [corrections and additions to the info on GBATEK](http://melonds.kuribo64.net/board/thread.php?id=13), and for the game database used in this emulator
- [StrikerX3](https://github.com/StrikerX3), for his research on 3D rendering on the DS
- [Simone Coco](https://github.com/CocoSimone), [Fleroviux](https://github.com/fleroviux), [Lady Starbreeze](https://github.com/LadyStarbreeze), [Merry](https://github.com/merryhime) and [Powerlated](https://github.com/Powerlated) for help throughout development
- The Emulation Development server on Discord as a whole, for providing several invaluable resources
