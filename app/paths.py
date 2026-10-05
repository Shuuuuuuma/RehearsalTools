"""ファイルの置き場所．

- 開発時（python run_tool.py）：どちらもプロジェクトのフォルダ
- exe にした時：
    RESOURCE_DIR = exe の中に同梱したファイル（画面の HTML・数字の見本）
    DATA_ROOT    = exe と同じフォルダ（設定・参照画像・周回結果・DB．exe を作り直しても消えない）
"""
import os
import sys

FROZEN = getattr(sys, "frozen", False)
PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESOURCE_DIR = getattr(sys, "_MEIPASS", PROJECT_DIR)
DATA_ROOT = os.path.dirname(os.path.abspath(sys.executable)) if FROZEN else PROJECT_DIR
