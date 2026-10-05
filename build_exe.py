"""ツールを 1 つの exe にまとめる．

使い方（gakumasu-tool フォルダで）:
    python build_exe.py            → dist\\GakumasRehearsalTool.exe
    python build_exe.py --admin    → 起動時に管理者権限を求める exe
    python build_exe.py --onedir   → 1 ファイルではなくフォルダ形式（起動が速い）
"""
import importlib.util
import os
import subprocess
import sys

# GitHub Actions など文字コードが UTF-8 でない環境でも日本語の表示で落ちないように
for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

HERE = os.path.dirname(os.path.abspath(__file__))
NAME = "GakumasRehearsalTool"


def has(mod: str) -> bool:
    try:
        return importlib.util.find_spec(mod) is not None
    except Exception:
        return False


def main():
    os.chdir(HERE)
    if not has("PyInstaller"):
        print("PyInstaller をインストールします...")
        subprocess.check_call([sys.executable, "-m", "pip", "install", "pyinstaller"])

    missing = [m for m in ("fastapi", "uvicorn", "PIL", "numpy", "pandas", "cv2", "matplotlib", "winocr", "win32clipboard")
               if not has(m)]
    if missing:
        print(f"必要なパッケージが足りません: {missing}\n先に  pip install -r requirements.txt  を実行してください．")
        sys.exit(1)

    sep = os.pathsep  # Windows では ";"
    args = [
        "run_tool.py",
        "--name", NAME,
        "--noconfirm", "--clean",
        "--console",  # ログを表示する黒い画面を出す（閉じると終了）
        "--add-data", f"static{sep}static",
        "--add-data", f"app/automation/digit_templates.npz{sep}app/automation",
        "--collect-submodules", "uvicorn",
        "--collect-submodules", "app",
        "--hidden-import", "win32clipboard",
        "--collect-all", "winocr",
    ]
    # winocr が使う Windows Runtime のパッケージ（入っているものだけ同梱）
    for pkg in ("winrt", "winsdk"):
        if has(pkg):
            args += ["--collect-all", pkg]
    args.append("--onedir" if "--onedir" in sys.argv else "--onefile")
    if "--admin" in sys.argv:
        args.append("--uac-admin")

    print("PyInstaller " + " ".join(args))
    import PyInstaller.__main__
    PyInstaller.__main__.run(args)

    out = os.path.join(HERE, "dist", NAME + ("" if "--onedir" in sys.argv else ".exe"))
    print("\n==================================================")
    print(f"完成: {out}")
    print("exe は好きなフォルダに置けます．設定・参照画像・周回結果は exe と同じフォルダに作られます．")
    print("今までのデータを引き継ぐ場合は automation_config.json / references / sessions / data を exe の横にコピーしてください．")
    print("==================================================")


if __name__ == "__main__":
    main()
