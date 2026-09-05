@echo off
REM Starts separate Whisper servers for push-to-talk and web recording processing.

call "%~dp0whisper-servers.bat"
if not defined WHISPER_DIR (
    echo whisper-servers.bat did not define WHISPER_DIR.
    exit /b 1
)

set "WHISPER_LOG_DIR=%APPDATA%\byovox\data\webui\logs"
if not exist "%WHISPER_LOG_DIR%" mkdir "%WHISPER_LOG_DIR%"

call :start_whisper "whisper-ptt" "%PTT_PORT%" "%PTT_MODEL%" "%PTT_LANGUAGE%" "%PTT_EXTRA_ARGS%"
call :start_whisper "whisper-processing" "%PROCESSING_PORT%" "%PROCESSING_MODEL%" "%PROCESSING_LANGUAGE%" "%PROCESSING_EXTRA_ARGS%"

REM Give both servers time to load their models before byovox probes them.
timeout /t 15 /nobreak >nul

"%~dp0target\debug\byovox.exe" quit >nul 2>&1
REM Give the old daemon time to release its single-instance socket before starting a new one.
timeout /t 2 /nobreak >nul
"%~dp0target\debug\byovox.exe"
exit /b 0

:start_whisper
set "SERVER_TITLE=%~1"
set "SERVER_PORT=%~2"
set "SERVER_MODEL=%~3"
set "SERVER_LANGUAGE=%~4"
set "SERVER_EXTRA=%~5"

REM Always replace the server on this port so profile changes take effect immediately.
for /f "tokens=5" %%P in ('netstat -ano ^| findstr /R /C:":%SERVER_PORT% .*LISTENING"') do (
    echo stopping %SERVER_TITLE% process %%P on port %SERVER_PORT%...
    taskkill /PID %%P /T /F >nul 2>&1
)
timeout /t 2 /nobreak >nul

if not exist "%SERVER_MODEL%" (
    echo %SERVER_TITLE% model not found: %SERVER_MODEL%
    exit /b 1
)

start "%SERVER_TITLE%" /b cmd /d /c ""%WHISPER_DIR%\whisper-server.exe" -m "%SERVER_MODEL%" --host 127.0.0.1 --port "%SERVER_PORT%" -l "%SERVER_LANGUAGE%" --inference-path /v1/audio/transcriptions %SERVER_EXTRA% > "%WHISPER_LOG_DIR%\%SERVER_TITLE%.log" 2>&1"
exit /b 0
