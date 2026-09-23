@echo off
setlocal EnableDelayedExpansion
title Aura Engine - High-Fidelity DSP Audio

echo ===================================================
echo  Aura Engine startup
echo ===================================================

REM Always work from the directory this .bat file lives in, no matter
REM where the user double-clicked it from.
cd /d "%~dp0"
set "ROOT=%CD%"

REM Show exactly what we're about to build so there is no doubt about
REM which copy of the project we're touching.
echo.
echo [info] script   : %~f0
echo [info] root dir : %ROOT%

cd "%ROOT%\src-tauri"
for /f "delims=" %%i in ('git rev-parse --short HEAD 2^>nul') do set "GIT_HASH=%%i"
for /f "delims=" %%i in ('git rev-parse --abbrev-ref HEAD 2^>nul') do set "GIT_BRANCH=%%i"
for /f "delims=" %%i in ('git log -1 --format^=%%s 2^>nul') do set "GIT_MSG=%%i"
if defined GIT_BRANCH (
    echo [info] git      : !GIT_BRANCH! @ !GIT_HASH!
    echo [info] last msg : !GIT_MSG!
) else (
    echo [warn] git info unavailable [not a git repo or git not on PATH]
)
echo.

set "EXE=%CD%\target\release\aura-engine.exe"

REM Optional --clean argument: delete the aura-engine build cache so the
REM next build starts from a known good state.
if /i "%1"=="--clean" (
    echo [info] --clean requested: removing aura-engine build artefacts...
    cargo clean -p aura-engine
    echo.
    goto do_build
)

REM Optional --build argument: force a cargo build even if nothing changed.
if /i "%1"=="--build" goto do_build

REM -- Fast path --
REM Skip cargo entirely when the existing binary is newer than every
REM tracked source file (Rust sources, frontend assets which are embedded
REM into the binary via tauri::generate_context!, shaders and configs).
REM Even a no-op cargo build costs ~0.5s, and any build-script rerun used
REM to bump OUT_DIR mtimes and force a full ~13s relink.
if not exist "%EXE%" goto do_build
powershell -NoProfile -ExecutionPolicy Bypass -Command "$exe=(Get-Item 'target\release\aura-engine.exe').LastWriteTimeUtc; $files=@(Get-ChildItem -Recurse -File -Path 'src','..\src' -ErrorAction SilentlyContinue); foreach($p in @('Cargo.toml','Cargo.lock','build.rs','tauri.conf.json','.cargo\config.toml')){ if(Test-Path $p){ $files += Get-Item $p } }; if($files | Where-Object { $_.LastWriteTimeUtc -gt $exe }){ exit 1 } else { exit 0 }"
if errorlevel 1 (
    echo [info] source changes detected - rebuilding...
    echo.
    goto do_build
)
echo [info] sources unchanged - launching existing binary [use --build to force a rebuild]
goto launch

:do_build
REM -- Step 1: explicit build pass --
REM cargo run would do this implicitly, but it hides the output behind
REM the launched application. Doing build first makes it obvious whether
REM ANY recompilation actually happened.
echo ===================================================
echo  Step 1/2  Compiling release binary...
echo ===================================================
echo.
cargo build --release --bin aura-engine
if errorlevel 1 (
    echo.
    echo ===================================================
    echo  BUILD FAILED - see compiler errors above
    echo ===================================================
    pause
    exit /b 1
)

if not exist "%EXE%" (
    echo [ERROR] expected binary not found: %EXE%
    pause
    exit /b 1
)

:launch
REM -- Step 2: launch the binary directly --
REM Bypassing cargo run here removes any ambiguity about which binary
REM is being executed and prints its mtime so you can verify it's fresh.
echo.
echo ===================================================
echo  Step 2/2  Launching Aura Engine
echo ===================================================
echo [info] binary   : %EXE%
for %%F in ("%EXE%") do echo [info] built    : %%~tF  [size %%~zF bytes]
echo.

"%EXE%"

echo.
echo [info] Aura Engine exited.
pause
endlocal
