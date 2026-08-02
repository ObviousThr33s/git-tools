py.exe -m PyInstaller --onefile --clean --noconfirm git_history_native.py
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue build, dist, *.spec; python -m PyInstaller --onefile --clean --noconfirm github_visualizer.py
