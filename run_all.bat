@echo off
setlocal
cd /d "%~dp0"

echo ===================================================
echo   Minecraft Auto-Enchanter - Launch All Accounts
echo ===================================================
echo.

if not exist ".env" (
    if exist ".env.example" (
        echo [.env] not found. Copying .env.example to .env...
        copy ".env.example" ".env" >nul
        echo Please edit .env with your Microsoft accounts, then press any key.
        pause
    )
)

echo Starting all configured accounts concurrently...
where cargo >nul 2>nul
if %ERRORLEVEL% EQU 0 (
    cargo run --release -- --all %*
) else if exist "target\release\Enchanter.exe" (
    target\release\Enchanter.exe --all %*
) else if exist "Enchanter.exe" (
    Enchanter.exe --all %*
) else (
    echo [ERROR] Neither Cargo nor Enchanter.exe was found!
    pause
)

if %ERRORLEVEL% NEQ 0 (
    echo.
    echo [ERROR] Enchanter exited with error code %ERRORLEVEL%.
    pause
)
