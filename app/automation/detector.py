"""画面状態の判定：ボタン領域を参照画像と比較して「今どの画面か」を判断する．"""
import os
from typing import Dict, List, Optional

import cv2
import numpy as np
from PIL import Image

from app.automation.config import ConfigStore


def crop_rel(img: Image.Image, region: List[float]) -> Image.Image:
    W, H = img.size
    x, y, w, h = region
    box = (int(round(x * W)), int(round(y * H)), int(round((x + w) * W)), int(round((y + h) * H)))
    box = (max(0, box[0]), max(0, box[1]), min(W, max(box[0] + 1, box[2])), min(H, max(box[1] + 1, box[3])))
    return img.crop(box)


def brightness(img: Image.Image) -> float:
    return float(np.asarray(img.convert("L"), dtype=np.float32).mean())


def similarity(a: Image.Image, ref: Image.Image) -> float:
    """0〜1 の類似度．グレースケールの正規化相関と色ヒストグラム相関の平均．"""
    a = a.convert("RGB").resize(ref.size, Image.Resampling.BILINEAR)
    ra = np.asarray(a)
    rr = np.asarray(ref.convert("RGB"))
    ga = cv2.cvtColor(ra, cv2.COLOR_RGB2GRAY).astype(np.float32)
    gr = cv2.cvtColor(rr, cv2.COLOR_RGB2GRAY).astype(np.float32)
    if ga.std() < 1e-3 or gr.std() < 1e-3:
        ncc = 1.0 - min(1.0, abs(ga.mean() - gr.mean()) / 64.0)
    else:
        ncc = float(cv2.matchTemplate(ga, gr, cv2.TM_CCOEFF_NORMED)[0][0])
    ha = cv2.calcHist([cv2.cvtColor(ra, cv2.COLOR_RGB2HSV)], [0, 1], None, [30, 32], [0, 180, 0, 256])
    hr = cv2.calcHist([cv2.cvtColor(rr, cv2.COLOR_RGB2HSV)], [0, 1], None, [30, 32], [0, 180, 0, 256])
    cv2.normalize(ha, ha)
    cv2.normalize(hr, hr)
    hist = float(cv2.compareHist(ha, hr, cv2.HISTCMP_CORREL))
    return (max(0.0, ncc) + max(0.0, hist)) / 2.0


def shifted_similarity(screen: Image.Image, region: List[float], ref: Image.Image,
                       mx: float = 0.02, my: float = 0.008) -> float:
    """判定領域を少し広げて参照画像の位置を探し，一番合う場所での類似度を返す．
    ウィンドウの大きさや画面の描画が数ピクセルずれても判定が崩れないようにするため．"""
    x, y, w, h = region
    big = crop_rel(screen, [x - mx, y - my, w + 2 * mx, h + 2 * my])
    base = crop_rel(screen, region)
    # 判定領域を参照画像の大きさに合わせる倍率で，広げた領域も縮尺する
    sx, sy = ref.width / max(1, base.width), ref.height / max(1, base.height)
    bw, bh = max(ref.width, int(round(big.width * sx))), max(ref.height, int(round(big.height * sy)))
    bigr = np.asarray(big.convert("RGB").resize((bw, bh), Image.Resampling.BILINEAR))
    gb = cv2.cvtColor(bigr, cv2.COLOR_RGB2GRAY).astype(np.float32)
    gr = cv2.cvtColor(np.asarray(ref.convert("RGB")), cv2.COLOR_RGB2GRAY).astype(np.float32)
    best = similarity(base, ref)
    if gr.std() < 1e-3:
        return best
    res = cv2.matchTemplate(gb, gr, cv2.TM_CCOEFF_NORMED)
    _, _, _, (bx, by) = cv2.minMaxLoc(res)
    patch = Image.fromarray(bigr[by:by + ref.height, bx:bx + ref.width])
    return max(best, similarity(patch, ref))


def orange_ratio(img: Image.Image) -> float:
    """「開始する」「終了」のようなオレンジボタンの画素割合（参照画像が無い時の代替判定）．"""
    a = np.asarray(img.convert("RGB")).astype(np.int16)
    r, g, b = a[..., 0], a[..., 1], a[..., 2]
    m = (r > 220) & (g > 70) & (g < 200) & (b < 110) & (r - b > 120)
    return float(m.mean())


class ScreenDetector:
    BUTTONS = ("start", "skip", "end", "confirm")

    def __init__(self, config: ConfigStore):
        self.config = config
        self._refs: Dict[str, Optional[Image.Image]] = {}
        self._ref_mtime: Dict[str, float] = {}

    def _reference(self, name: str) -> Optional[Image.Image]:
        cfg = self.config.get()["buttons"][name]
        path = ConfigStore.reference_path(cfg["reference"])
        if not os.path.exists(path):
            self._refs[name] = None
            return None
        mt = os.path.getmtime(path)
        if self._ref_mtime.get(name) != mt:
            self._refs[name] = Image.open(path).convert("RGB")
            self._ref_mtime[name] = mt
        return self._refs[name]

    def has_reference(self, name: str) -> bool:
        return self._reference(name) is not None

    def save_reference(self, name: str, screen: Image.Image) -> str:
        cfg = self.config.get()["buttons"][name]
        path = ConfigStore.reference_path(cfg["reference"])
        crop_rel(screen, cfg["region"]).save(path)
        self._ref_mtime.pop(name, None)
        return path

    def check(self, name: str, screen: Image.Image) -> Dict[str, object]:
        """ボタンが見えているかを判定し，判定に使った数値も返す．"""
        conf = self.config.get()
        bcfg = conf["buttons"][name]
        region = crop_rel(screen, bcfg["region"])
        ref = self._reference(name)
        info: Dict[str, object] = {"button": name, "brightness": round(brightness(region), 1)}
        thr = float(conf["match_threshold"])

        if ref is not None:
            sim = shifted_similarity(screen, bcfg["region"], ref)
            info["similarity"] = round(sim, 3)
            visible = sim >= thr
        elif name in ("start", "end", "confirm"):
            ratio = orange_ratio(region)
            info["orange_ratio"] = round(ratio, 3)
            info["note"] = "参照画像なし（色で代替判定）"
            visible = ratio >= 0.45
        else:
            # スキップ：暗いボタン地に白文字・白枠．白が少しだけ・暗部が大半，という特徴で代替判定
            a = np.asarray(region.convert("RGB")).astype(np.int16)
            white = float((a.min(axis=2) > 200).mean())
            dark = float((a.max(axis=2) < 90).mean())
            info.update(white_ratio=round(white, 3), dark_ratio=round(dark, 3), note="参照画像なし（形で代替判定）")
            visible = 0.04 <= white <= 0.3 and dark >= 0.5

        if name == "skip":
            visible = visible and info["brightness"] >= float(bcfg.get("brightness_min", 0))
        if name == "confirm":
            # 確認ダイアログは左に白い「キャンセル」がある．これが無ければダイアログではない
            a = np.asarray(crop_rel(screen, bcfg["cancel_region"]).convert("RGB")).astype(np.int16)
            cw = float((a.min(axis=2) > 225).mean())
            info["cancel_white"] = round(cw, 3)
            visible = visible and cw >= 0.5
        info["visible"] = bool(visible)
        return info

    def check_preparing(self, screen: Image.Image) -> Dict[str, object]:
        """右下の「コンテスト準備中...」が出ているか（白文字の画素割合で判定）．"""
        pcfg = self.config.get()["preparing"]
        a = np.asarray(crop_rel(screen, pcfg["region"]).convert("RGB")).astype(np.int16)
        white = float((a.min(axis=2) > 215).mean())
        return {"button": "preparing", "white_ratio": round(white, 3), "visible": white >= float(pcfg["white_min"])}

    def check_all(self, screen: Image.Image) -> Dict[str, Dict[str, object]]:
        out = {n: self.check(n, screen) for n in self.BUTTONS}
        out["preparing"] = self.check_preparing(screen)
        if out["preparing"]["visible"]:
            out["skip"]["visible"] = False  # 準備中はスキップを押せる状態とみなさない
        if out["confirm"]["visible"]:
            # ダイアログの下のボタンは押せないので見えていない扱い
            for k in ("start", "skip", "end"):
                out[k]["visible"] = False
        return out

    # ---- 編成（9 枠アイコン）の比較 ----
    def lineup_icons(self, screen: Image.Image) -> List[Image.Image]:
        return [crop_rel(screen, r) for r in self.config.get()["regions"]["icons"]]

    @staticmethod
    def lineup_distance(a: Image.Image, b: Image.Image, icons: List[List[float]]) -> List[float]:
        d = []
        for r in icons:
            ia = np.asarray(crop_rel(a, r).convert("L").resize((16, 16)), dtype=np.float32)
            ib = np.asarray(crop_rel(b, r).convert("L").resize((16, 16)), dtype=np.float32)
            d.append(float(np.abs(ia - ib).mean()))
        return d
