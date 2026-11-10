@echo off
rem Build Inti and boot it in a QEMU window. Needs Rust, Python and QEMU; see README.md.
python "%~dp0build.py" run %*
