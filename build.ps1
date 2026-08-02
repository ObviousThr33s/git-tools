# Build the single-file executable that run.bat launches: dist\git_history_native.exe
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue build, dist, *.spec
py.exe -m PyInstaller --onefile --clean --noconfirm git_history_native.py
