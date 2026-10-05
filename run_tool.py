import socket
import sys
import threading
import time
import webbrowser

HOST, PORT = "127.0.0.1", 8000
URL = f"http://localhost:{PORT}/automation"


def port_in_use() -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.3)
        return s.connect_ex((HOST, PORT)) == 0


def open_browser():
    time.sleep(1.5)
    print(f"\nブラウザで自動周回ページを開きます: {URL}")
    print(f"（集計ダッシュボード: http://localhost:{PORT} ）")
    webbrowser.open(URL)


def main():
    print("==================================================")
    print("🌸 学マス リハーサル結果自動集計＆分析ツール 🌸")
    print("==================================================")

    # すでに起動中なら，二重に起動せずブラウザだけ開く
    if port_in_use():
        print("すでに起動しています．ブラウザを開きます．")
        webbrowser.open(URL)
        time.sleep(2)
        return

    try:
        import ctypes
        if not ctypes.windll.shell32.IsUserAnAdmin():
            print("⚠ 管理者として実行されていません．ゲームが管理者権限で動いているとクリックが届きません．")
            print("  うまくクリックされない場合は，右クリック →「管理者として実行」で起動してください．")
    except Exception:
        pass
    print("ホットキー: Ctrl+Shift+A 開始 / Ctrl+Shift+Q 中断 / Ctrl+Shift+S スクショ")
    print("このウィンドウを閉じるとツールが終了します．")
    print("サーバーを起動しています...")

    import uvicorn
    from app.main import app  # exe でも動くよう，文字列ではなく直接読み込む

    threading.Thread(target=open_browser, daemon=True).start()
    uvicorn.run(app, host=HOST, port=PORT, log_level="warning")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        import traceback
        traceback.print_exc()
        print(f"\n起動に失敗しました: {e}")
        if getattr(sys, "frozen", False):
            input("Enter キーで閉じます...")
