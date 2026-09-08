@echo off
setlocal EnableExtensions
cd /d "%~dp0"
title Vibe Mod - Dev Launcher

set "PNPM_VERSION=10.4.1"
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"

cls
echo ============================================================
echo                    VIBE MOD - DEV LAUNCHER
echo ============================================================
echo.
echo Este launcher prepara o ambiente e inicia o Vibe Mod em modo dev.
echo Na primeira execucao ele pode instalar dependencias ausentes.
echo.

call :refresh_path
call :ensure_node || goto :error
call :ensure_pnpm || goto :error
call :ensure_uv || goto :error
call :ensure_rust || goto :error
call :ensure_msvc || goto :error
call :refresh_path
call :validate || goto :error

echo.
echo [VIBE] 1/3 Preparando Sona e FFmpeg...
echo.
uv run scripts/pre_build.py
if errorlevel 1 goto :runtime_error

echo.
echo [VIBE] 2/3 Instalando/atualizando dependencias do frontend...
echo.
pushd desktop
call pnpm install
if errorlevel 1 (
    set "EXIT_CODE=%ERRORLEVEL%"
    popd
    goto :runtime_error
)

echo.
echo [VIBE] 3/3 Iniciando Vibe Mod em modo Tauri dev...
echo [VIBE] O primeiro start pode demorar mais por causa da compilacao Rust.
echo.
call pnpm exec tauri dev
set "EXIT_CODE=%ERRORLEVEL%"
popd

if not "%EXIT_CODE%"=="0" goto :runtime_error

echo.
echo [OK] Vibe Mod encerrado normalmente.
exit /b 0

:ensure_node
where node >nul 2>nul && where npm >nul 2>nul && (
    for /f "tokens=*" %%V in ('node --version') do echo [OK] Node %%V
    exit /b 0
)

echo [SETUP] Node.js nao encontrado. Instalando Node.js LTS...
call :require_winget || exit /b 1
winget install -e --id OpenJS.NodeJS.LTS --accept-package-agreements --accept-source-agreements
if errorlevel 1 (
    echo [ERRO] Nao foi possivel instalar o Node.js automaticamente.
    exit /b 1
)
call :refresh_path
where node >nul 2>nul || (
    echo [ERRO] Node.js foi instalado, mas ainda nao apareceu no PATH.
    echo Feche este launcher, abra novamente e tente outra vez.
    exit /b 1
)
exit /b 0

:ensure_pnpm
set "CURRENT_PNPM="
for /f "tokens=*" %%V in ('pnpm --version 2^>nul') do set "CURRENT_PNPM=%%V"

if "%CURRENT_PNPM%"=="%PNPM_VERSION%" (
    echo [OK] pnpm %CURRENT_PNPM%
    exit /b 0
)

if defined CURRENT_PNPM (
    echo [SETUP] pnpm %CURRENT_PNPM% encontrado, mas o projeto usa %PNPM_VERSION%.
    echo [SETUP] Ajustando pnpm para %PNPM_VERSION%...
) else (
    echo [SETUP] pnpm nao encontrado. Instalando pnpm %PNPM_VERSION%...
)

where npm >nul 2>nul || (
    echo [ERRO] npm nao esta disponivel para instalar o pnpm.
    exit /b 1
)

call npm install --global pnpm@%PNPM_VERSION%
if errorlevel 1 (
    echo [ERRO] Nao foi possivel instalar pnpm %PNPM_VERSION%.
    exit /b 1
)
call :refresh_path

set "CURRENT_PNPM="
for /f "tokens=*" %%V in ('pnpm --version 2^>nul') do set "CURRENT_PNPM=%%V"
if not "%CURRENT_PNPM%"=="%PNPM_VERSION%" (
    echo [ERRO] Era esperado pnpm %PNPM_VERSION%, mas foi encontrado %CURRENT_PNPM%.
    exit /b 1
)
echo [OK] pnpm %CURRENT_PNPM%
exit /b 0

:ensure_uv
where uv >nul 2>nul && (
    for /f "tokens=*" %%V in ('uv --version') do echo [OK] %%V
    exit /b 0
)

echo [SETUP] uv nao encontrado. Instalando...
where winget >nul 2>nul && (
    winget install -e --id astral-sh.uv --accept-package-agreements --accept-source-agreements
)
call :refresh_path
where uv >nul 2>nul && exit /b 0

echo [SETUP] Tentando instalador oficial do uv...
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://astral.sh/uv/install.ps1 ^| iex"
if errorlevel 1 exit /b 1
call :refresh_path
where uv >nul 2>nul || (
    echo [ERRO] uv foi instalado, mas nao apareceu no PATH.
    echo Feche este launcher, abra novamente e tente outra vez.
    exit /b 1
)
exit /b 0

:ensure_rust
where cargo >nul 2>nul && where rustc >nul 2>nul && (
    for /f "tokens=*" %%V in ('rustc --version') do echo [OK] %%V
    exit /b 0
)

echo [SETUP] Rust/Cargo nao encontrado. Instalando rustup...
call :require_winget || exit /b 1
winget install -e --id Rustlang.Rustup --accept-package-agreements --accept-source-agreements
if errorlevel 1 (
    echo [ERRO] Nao foi possivel instalar o Rust automaticamente.
    exit /b 1
)
call :refresh_path
where rustup >nul 2>nul && rustup default stable
call :refresh_path
where cargo >nul 2>nul || (
    echo [ERRO] Rust foi instalado, mas Cargo ainda nao apareceu no PATH.
    echo Feche este launcher, abra novamente e tente outra vez.
    exit /b 1
)
exit /b 0

:ensure_msvc
set "VS_PATH="
if exist "%VSWHERE%" (
    for /f "usebackq tokens=*" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VS_PATH=%%I"
)

if defined VS_PATH (
    echo [OK] Microsoft C++ Build Tools encontrado.
    exit /b 0
)

echo [SETUP] Microsoft C++ Build Tools nao encontrado.
echo [SETUP] Instalando o workload C++ necessario para Tauri/Rust...
call :require_winget || exit /b 1
winget install -e --id Microsoft.VisualStudio.2022.BuildTools --accept-package-agreements --accept-source-agreements --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
if errorlevel 1 (
    echo [ERRO] Nao foi possivel instalar o Microsoft C++ Build Tools.
    exit /b 1
)

echo [OK] Build Tools instalado.
exit /b 0

:validate
echo.
echo [CHECK] Validando ambiente...
where node >nul 2>nul || exit /b 1
where npm >nul 2>nul || exit /b 1
where pnpm >nul 2>nul || exit /b 1
where uv >nul 2>nul || exit /b 1
where cargo >nul 2>nul || exit /b 1
where rustc >nul 2>nul || exit /b 1
if not exist "scripts\pre_build.py" (
    echo [ERRO] scripts\pre_build.py nao foi encontrado.
    exit /b 1
)
if not exist "desktop\package.json" (
    echo [ERRO] desktop\package.json nao foi encontrado.
    exit /b 1
)
echo [OK] Ambiente pronto.
exit /b 0

:require_winget
where winget >nul 2>nul && exit /b 0
echo [ERRO] winget nao foi encontrado neste Windows.
echo Instale/atualize o "App Installer" da Microsoft Store e execute novamente.
exit /b 1

:refresh_path
set "PATH=%ProgramFiles%\nodejs;%USERPROFILE%\.cargo\bin;%USERPROFILE%\.local\bin;%APPDATA%\npm;%LOCALAPPDATA%\Microsoft\WinGet\Links;%PATH%"
exit /b 0

:runtime_error
if not defined EXIT_CODE set "EXIT_CODE=%ERRORLEVEL%"
echo.
echo ============================================================
echo [ERRO] O Vibe Mod nao conseguiu iniciar. Codigo: %EXIT_CODE%
echo ============================================================
echo.
echo A mensagem acima indica a etapa que falhou.
pause
exit /b %EXIT_CODE%

:error
echo.
echo ============================================================
echo [ERRO] Nao foi possivel preparar o ambiente do Vibe Mod.
echo ============================================================
echo.
echo Corrija a mensagem indicada acima e execute este arquivo novamente.
pause
exit /b 1
