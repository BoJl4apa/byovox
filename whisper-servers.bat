@echo off
REM Editable Whisper server profiles used by start-byovox.bat.
REM Keep the live dictation and long-recording processing servers separate.

set "WHISPER_DIR=C:\Workfold\aitools\whisper"

REM Push-to-talk: optimize for short interactive requests.
set "PTT_MODEL=%WHISPER_DIR%\ggml-base.bin"
set "PTT_PORT=8770"
set "PTT_LANGUAGE=auto"
set "PTT_EXTRA_ARGS="

REM Web UI processing: choose a larger/slower model or other server flags here.
REM The default is kept runnable with the model used by the existing setup.
set "PROCESSING_MODEL=%WHISPER_DIR%\ggml-base.bin"
set "PROCESSING_PORT=8771"
set "PROCESSING_LANGUAGE=auto"
set "PROCESSING_EXTRA_ARGS=-t 10"