"""リハーサル結果画面の数字読み取りと検算．

学マスの結果画面は数字のフォントが決まっているので，汎用 OCR ではなく
「数字 1 文字ずつ切り出して，見本（テンプレート）と比べる」方式で読む．

各ステージについて
  - 3 人のスコア（白文字）
  - ステージ合計（白文字・大）
  - 1 位ボーナス（1 位のアイドルの上に出る白い吹き出しの青文字 "+12345"．位置は 1 位の列によって変わる）
を読み，「3 人のスコア + ボーナス = ステージ合計」で検算する．
合わない場合は，しきい値を変えた読み直しと「2 番目に似ている数字」への置き換えを試し，
式が成り立つ最も自然な組み合わせを採用する（= repaired）．それでもだめなら flagged．
"""
import asyncio
import itertools
import os
import re
import threading
from typing import Any, Dict, List, Optional, Tuple

import cv2
import numpy as np
from PIL import Image, ImageOps

from app.automation.detector import crop_rel

SCORE_MIN, SCORE_MAX = 1, 9_999_999     # キャラが少ない・弱いと 3 桁のスコアもある
TOTAL_MIN, TOTAL_MAX = 1, 29_999_999

from app.paths import DATA_ROOT, RESOURCE_DIR

BUILTIN_TEMPLATES = os.path.join(RESOURCE_DIR, "app", "automation", "digit_templates.npz")
USER_TEMPLATES = os.path.join(DATA_ROOT, "data", "digit_templates_user.npz")
GW, GH = 12, 16  # 正規化した 1 文字の大きさ


# --------------------------------------------------------------------------
# 二値化
# --------------------------------------------------------------------------
def white_text_mask(img: Image.Image, threshold: int) -> np.ndarray:
    """白い文字 → True．"""
    a = np.asarray(img.convert("RGB")).astype(np.int16)
    return a.min(axis=2) >= threshold


def blue_text_mask(img: Image.Image, blue_min: int, margin: int) -> np.ndarray:
    """白い吹き出しの中の青文字 → True．吹き出しの外（アイドル画像の青など）は捨てる．"""
    a = np.asarray(img.convert("RGB")).astype(np.int16)
    r, b = a[..., 0], a[..., 2]
    blue = (b >= blue_min) & (b - r >= margin)
    white = (a.min(axis=2) >= 225).astype(np.uint8)
    # 文字の隙間を埋めて吹き出し全体を 1 つの塊にする
    k = max(3, a.shape[0] // 3)
    filled = cv2.morphologyEx(white | blue.astype(np.uint8), cv2.MORPH_CLOSE, np.ones((k, k), np.uint8))
    n, lab, st, _ = cv2.connectedComponentsWithStats(filled, 8)
    best, best_blue = None, 0
    for i in range(1, n):
        x, y, w, h, _ = st[i]
        if w < h * 2:  # 吹き出しは横長
            continue
        nb = int(blue[y:y + h, x:x + w].sum())
        if nb > best_blue:
            best, best_blue = (x, y, w, h), nb
    out = np.zeros_like(blue)
    if best is not None and best_blue >= 8:
        x, y, w, h = best
        out[y:y + h, x:x + w] = blue[y:y + h, x:x + w]
    return out


# --------------------------------------------------------------------------
# 1 文字ずつの切り出しと正規化
# --------------------------------------------------------------------------
def split_glyphs(mask: np.ndarray, kind: str) -> List[np.ndarray]:
    """数字 1 文字ずつの 2 値画像を左から順に返す（カンマ・"Pt"・"+" は除く）．"""
    ink = mask.astype(np.uint8)
    n, lab, st, _ = cv2.connectedComponentsWithStats(ink, 8)
    if n <= 1:
        return []
    hmax = st[1:, cv2.CC_STAT_HEIGHT].max()
    ratio = 0.85 if kind == "bonus" else 0.7
    keep = [i for i in range(1, n)
            if st[i, cv2.CC_STAT_HEIGHT] >= ratio * hmax and st[i, cv2.CC_STAT_AREA] >= 6]
    out = []
    for i in sorted(keep, key=lambda i: st[i, cv2.CC_STAT_LEFT]):
        x, y, w, h, _ = st[i]
        g = (lab[y:y + h, x:x + w] == i).astype(np.uint8)
        exp = max(1.0, 0.72 * h)
        parts = int(round(w / exp))
        if w > 1.45 * exp and parts >= 2:
            # 隣の数字とくっついている → インクの少ない列で分割
            prof = g.sum(axis=0).astype(float)
            cuts = [0]
            for j in range(1, parts):
                c = int(round(w * j / parts))
                lo, hi = max(1, c - 3), min(w - 1, c + 4)
                cuts.append(lo + int(np.argmin(prof[lo:hi])) if hi > lo else c)
            cuts.append(w)
            out.extend(g[:, a:b] for a, b in zip(cuts, cuts[1:]) if b > a)
        else:
            out.append(g)
    return out


def normalize_glyph(g: np.ndarray) -> np.ndarray:
    ys, xs = np.where(g > 0)
    if len(xs) == 0:
        return np.zeros(GW * GH, np.float32)
    g = g[ys.min():ys.max() + 1, xs.min():xs.max() + 1]
    h, w = g.shape
    bw = max(w, int(round(h * 0.8)))
    box = np.zeros((h, bw), np.uint8)
    off = (bw - w) // 2
    box[:, off:off + w] = g
    return cv2.resize(box.astype(np.float32), (GW, GH), interpolation=cv2.INTER_AREA).ravel()


class DigitTemplates:
    """数字の見本．内蔵の見本＋使っているうちに検算 OK だった数字を少しずつ追加（解像度の違いに追従）．"""

    PER_DIGIT_USER = 40

    def __init__(self):
        self._lock = threading.Lock()
        X, Y = [], []
        for p in (BUILTIN_TEMPLATES, USER_TEMPLATES):
            if os.path.exists(p):
                d = np.load(p)
                X.append(d["X"].astype(np.float32))
                Y.append(d["Y"].astype(np.int8))
        self.X = np.concatenate(X) if X else np.zeros((0, GW * GH), np.float32)
        self.Y = np.concatenate(Y) if Y else np.zeros((0,), np.int8)
        self._user_X: List[np.ndarray] = []
        self._user_Y: List[int] = []
        if os.path.exists(USER_TEMPLATES):
            d = np.load(USER_TEMPLATES)
            self._user_X = list(d["X"].astype(np.float32))
            self._user_Y = list(d["Y"].astype(int))
        self._dirty = 0

    @property
    def available(self) -> bool:
        return len(self.Y) > 0 and len(set(self.Y.tolist())) == 10

    def classify(self, f: np.ndarray) -> List[Tuple[int, float]]:
        """数字ごとの最小距離を近い順に返す．"""
        dist = ((self.X - f) ** 2).sum(axis=1)
        best = {}
        for d, v in zip(self.Y.tolist(), dist.tolist()):
            if d not in best or v < best[d]:
                best[d] = v
        return sorted(best.items(), key=lambda t: t[1])

    def learn(self, feats: List[np.ndarray], digits: str):
        with self._lock:
            for f, d in zip(feats, digits):
                d = int(d)
                idx = [i for i, y in enumerate(self._user_Y) if y == d]
                if len(idx) >= self.PER_DIGIT_USER:
                    del self._user_X[idx[0]]
                    del self._user_Y[idx[0]]
                self._user_X.append(f)
                self._user_Y.append(d)
            self._dirty += 1
            if self._dirty >= 5:
                self.flush()

    def flush(self):
        if not self._user_Y:
            return
        os.makedirs(os.path.dirname(USER_TEMPLATES), exist_ok=True)
        np.savez_compressed(USER_TEMPLATES, X=np.array(self._user_X, np.float32), Y=np.array(self._user_Y, np.int8))
        self._dirty = 0


# --------------------------------------------------------------------------
# エンジン（テンプレート照合が既定．見本が無い場合のみ winocr / Tesseract）
# --------------------------------------------------------------------------
class OcrEngine:
    def __init__(self, engine: str = "auto", lang: str = "ja"):
        self.lang = lang
        self.templates = DigitTemplates()
        self.name = None
        self._local = threading.local()
        if engine in ("auto", "template") and self.templates.available:
            self.name = "template"
            return
        candidates = ["winocr", "tesseract"] if engine in ("auto", "template") else [engine]
        for c in candidates:
            try:
                if c == "winocr":
                    import winocr  # noqa: F401
                else:
                    import pytesseract
                    pytesseract.get_tesseract_version()
                self.name = c
                break
            except Exception:
                continue
        if self.name is None:
            raise RuntimeError("数字の見本（digit_templates.npz）も OCR エンジンも見つかりません")

    def _loop(self):
        loop = getattr(self._local, "loop", None)
        if loop is None:
            loop = asyncio.new_event_loop()
            self._local.loop = loop
        return loop

    def read_line(self, img: Image.Image) -> str:
        if self.name == "winocr":
            import winocr
            res = self._loop().run_until_complete(winocr.recognize_pil(img.convert("RGB"), self.lang))
            return " ".join(line.text for line in res.lines)
        import pytesseract
        return pytesseract.image_to_string(img, config="--psm 7 -c tessedit_char_whitelist=0123456789").strip()


def _glyphs_to_image(glyphs: List[np.ndarray], scale: int = 3) -> Optional[Image.Image]:
    if not glyphs:
        return None
    h = max(g.shape[0] for g in glyphs)
    gap = max(2, h // 4)
    canvas = np.zeros((h, sum(g.shape[1] for g in glyphs) + gap * (len(glyphs) - 1)), np.uint8)
    x = 0
    for g in glyphs:
        canvas[h - g.shape[0]:, x:x + g.shape[1]] = g
        x += g.shape[1] + gap
    img = Image.fromarray(np.where(canvas > 0, 0, 255).astype(np.uint8), "L")
    img = img.resize((img.width * scale, img.height * scale), Image.Resampling.LANCZOS)
    return ImageOps.expand(img, border=24, fill=255).convert("RGB")


# --------------------------------------------------------------------------
# 結果画面の読み取り
# --------------------------------------------------------------------------
class FieldRead:
    """1 つの数値欄の読み取り結果．候補 = [(値, コスト)]，コストが小さいほど確からしい．"""

    def __init__(self, value: Optional[int], cands: List[Tuple[int, float]], feats=None, digits: str = "",
                 xcenter: Optional[float] = None, empty: bool = False):
        self.value = value
        self.empty = empty  # 空き枠（スコアの代わりに「ー」が表示されている）
        self.xcenter = xcenter  # 欄の中での文字の横位置（0〜1）．ボーナスの吹き出しが何列目かに使う
        self.cands = cands
        self.feats = feats or []
        self.digits = digits


class ResultReader:
    def __init__(self, engine: OcrEngine):
        self.engine = engine

    # ---- 1 欄 ----
    def _read_field(self, crop: Image.Image, kind: str, thr: int, ocfg: Dict[str, Any]) -> FieldRead:
        mask = blue_text_mask(crop, ocfg["bonus_blue_min"], ocfg["bonus_margin"]) if kind == "bonus" \
            else white_text_mask(crop, thr)
        if kind == "score" and self._is_dash(mask):
            return FieldRead(0, [(0, 0.0)], empty=True)
        glyphs = split_glyphs(mask, kind)
        if not glyphs:
            return FieldRead(None, [])
        xs = np.where(mask.any(axis=0))[0]
        xcenter = float((xs.min() + xs.max()) / 2 / mask.shape[1]) if len(xs) else None
        if self.engine.name != "template":
            img = _glyphs_to_image(glyphs)
            txt = re.sub(r"\D", "", self.engine.read_line(img)) if img else ""
            v = int(txt) if txt else None
            return FieldRead(v, [(v, 0.0)] if v is not None else [], xcenter=xcenter)

        feats = [normalize_glyph(g) for g in glyphs]
        ranked = [self.engine.templates.classify(f) for f in feats]
        digits = "".join(str(r[0][0]) for r in ranked)
        value = int(digits)
        cands = [(value, 0.0)]
        # 1 文字だけ 2〜3 番目の候補に置き換えた値（コスト＝距離の増分）
        for i, r in enumerate(ranked):
            for d, dist in r[1:3]:
                alt = digits[:i] + str(d) + digits[i + 1:]
                cands.append((int(alt), dist - r[0][1]))
        # 2 文字置き換え（それぞれの 2 番目の候補）
        for i, j in itertools.combinations(range(len(ranked)), 2):
            ri, rj = ranked[i], ranked[j]
            if len(ri) > 1 and len(rj) > 1:
                alt = list(digits)
                alt[i], alt[j] = str(ri[1][0]), str(rj[1][0])
                cands.append((int("".join(alt)), (ri[1][1] - ri[0][1]) + (rj[1][1] - rj[0][1])))
        cands.sort(key=lambda t: t[1])
        return FieldRead(value, cands, feats, digits, xcenter)

    @staticmethod
    def _is_dash(mask: np.ndarray) -> bool:
        """空き枠の「ー」か．数字より明らかに背が低く横長の白い線だけがある状態．"""
        ys, xs = np.where(mask)
        if len(xs) < 4:
            return False
        h = ys.max() - ys.min() + 1
        w = xs.max() - xs.min() + 1
        return h <= 0.35 * mask.shape[0] and w >= 2.5 * h

    def _score_crops(self, screen, regions, stage):
        x, y, w, h = regions["score"][stage]
        cols = regions["score_columns"]
        return [crop_rel(screen, [cols[i], y, cols[i + 1] - cols[i], h]) for i in range(3)]

    def read(self, screen: Image.Image, cfg: Dict[str, Any]) -> Dict[str, Any]:
        ocfg, regions = cfg["ocr"], cfg["regions"]
        stages = [self._read_stage(screen, s, ocfg, regions) for s in range(3)]
        statuses = [st["status"] for st in stages]
        overall = "flagged" if "flagged" in statuses else ("repaired" if "repaired" in statuses else "ok")
        flags = ";".join(f"S{i + 1}:{st['flag']}" for i, st in enumerate(stages) if st["flag"])
        return {"stages": stages, "status": overall, "flags": flags}

    def _read_stage(self, screen, s, ocfg, regions) -> Dict[str, Any]:
        """ゲームのルール
            ボーナス = 最高スコア ÷ 5（切り捨て）
            合計     = 3 人のスコア + ボーナス
        を使って検算・補正する．ボーナス欄の読み取りは補助（吹き出しの位置＝最高スコアの列も使う）．"""
        st, tt = int(ocfg["score_threshold"]), int(ocfg["total_threshold"])
        score_crops = self._score_crops(screen, regions, s)
        total_crop = crop_rel(screen, regions["total"][s])
        bregion = regions["bonus"][s]
        bonus_crop = crop_rel(screen, bregion)

        sf = [self._read_field(c, "score", st, ocfg) for c in score_crops]
        tf = self._read_field(total_crop, "total", tt, ocfg)
        bf = self._read_field(bonus_crop, "bonus", 0, ocfg)
        primary = {"scores": [f.value for f in sf], "total": tf.value, "bonus": bf.value}

        # 吹き出しの位置から「1 位の列」を推定
        top_col = None
        if bf.xcenter is not None:
            x = bregion[0] + bf.xcenter * bregion[2]
            cols = regions["score_columns"]
            for i in range(3):
                if cols[i] <= x < cols[i + 1]:
                    top_col = i

        empty = [f.empty for f in sf]

        def done(sc, status, flag):
            mx = max(sc)
            shown = [None if empty[i] else v for i, v in enumerate(sc)]  # 空き枠は空欄で記録
            out = {"scores": shown, "total": sum(sc) + mx // 5, "bonus": mx // 5, "status": status, "flag": flag}
            if status != "ok":
                out["raw"] = primary
            return out

        def consistent(sc, t):
            ok = all((empty[i] and v == 0) or SCORE_MIN <= v <= SCORE_MAX for i, v in enumerate(sc))
            return ok and t is not None and sum(sc) + max(sc) // 5 == t

        sc0 = [f.value for f in sf]
        if all(empty):  # 3 人とも空き（ステージに誰もいない）
            return done([0, 0, 0], "ok", "empty_stage")
        if all(v is not None for v in sc0) and consistent(sc0, tf.value):
            bonus_ok = bf.value is None or bf.value == max(sc0) // 5
            col_ok = top_col is None or sc0[top_col] == max(sc0)
            if bonus_ok and col_ok:
                self._learn(sf + [tf] + ([bf] if bf.value is not None else []))
            return done(sc0, "ok", "")

        # ---- 候補を集める（しきい値を変えた読み直しも候補に入れる）----
        s_c = [list(f.cands) for f in sf]
        t_c = list(tf.cands)
        for d in (-30, 30, -55):
            for i, c in enumerate(score_crops):
                f = self._read_field(c, "score", st + d, ocfg)
                if f.value is not None:
                    s_c[i].append((f.value, 0.4))
            f = self._read_field(total_crop, "total", tt + d, ocfg)
            if f.value is not None:
                t_c.append((f.value, 0.4))

        def top(c, lo, hi, n=10):
            best: Dict[int, float] = {}
            for v, cost in c:
                if lo <= v <= hi and (v not in best or cost < best[v]):
                    best[v] = cost
            return sorted(best.items(), key=lambda t: t[1])[:n]

        s_c = [[(0, 0.0)] if empty[i] else top(c, SCORE_MIN, SCORE_MAX) for i, c in enumerate(s_c)]
        t_c = top(t_c, TOTAL_MIN, TOTAL_MAX, 12)
        t_map = dict(t_c)
        b_map = dict(top(bf.cands, 1, SCORE_MAX))

        def extra_cost(sc) -> Optional[float]:
            """補正候補の追加チェック．ボーナス欄が読めていれば「最高スコア÷5」と一致すること，
            吹き出しの位置が分かれば その列が最高スコアであることを必須にする（None = 不採用）．"""
            mx = max(sc)
            if top_col is not None and sc[top_col] != mx:
                return None
            if b_map:
                return b_map.get(mx // 5)
            return 1.0  # ボーナス欄が読めない場合は少し減点

        found: List[Tuple[float, List[int], str]] = []

        # A) 3 人とも候補あり：合計の候補と一致する組み合わせ
        if all(s_c):
            for combo in itertools.product(*s_c):
                sc = [v for v, _ in combo]
                t = sum(sc) + max(sc) // 5
                ec = extra_cost(sc)
                if t in t_map and ec is not None:
                    found.append((sum(c for _, c in combo) + t_map[t] + ec, sc, "reread"))

        # B) 1 人だけ読めない／候補が全部外れ：合計から逆算
        for miss in range(3):
            if empty[miss]:
                continue
            others_c = [s_c[j] for j in range(3) if j != miss]
            if not all(others_c):
                continue
            for oc in itertools.product(*others_c):
                ov = [v for v, _ in oc]
                ocost = sum(c for _, c in oc)
                for t, tcost in t_c:
                    rest = t - sum(ov)
                    sols = set()
                    m = rest - max(ov) // 5          # 読めない人が 1 位ではない場合
                    if SCORE_MIN <= m <= max(ov) and max(ov) > 0:
                        sols.add(m)
                    for m in range((rest * 5) // 6 - 2, (rest * 5) // 6 + 3):  # 1 位の場合 m + m//5 = rest
                        if m >= max(ov) and m + m // 5 == rest:
                            sols.add(m)
                    for m in sols:
                        sc = ov[:miss] + [m] + ov[miss:]
                        ec = extra_cost(sc)
                        # 逆算は式 1 本で必ず解けてしまうので，ボーナス欄との一致を必須にする
                        if ec is not None and b_map:
                            found.append((ocost + tcost + 2.5 + ec, sc, f"derived_C{miss + 1}"))

        # C) 合計が読めない：3 人のスコアから計算（ボーナス欄が一致する場合のみ）
        if not t_c and all(s_c) and b_map:
            for combo in itertools.product(*s_c):
                sc = [v for v, _ in combo]
                ec = extra_cost(sc)
                if ec is not None:
                    found.append((sum(c for _, c in combo) + 2.5 + ec, sc, "total_derived"))

        found = [f for f in found if f[0] <= 8.0]
        if found:
            found.sort(key=lambda t: t[0])
            cost, sc, flag = found[0]
            # 同じくらい確からしい別解があれば念のため要確認に回す
            rivals = [f for f in found[1:] if f[1] != sc and f[0] - cost < 0.15]
            if rivals and cost > 0.5:
                return done(sc, "flagged", flag + "_ambiguous")
            return done(sc, "repaired", flag)

        sc = [None if empty[i] else (c[0][0] if c else None) for i, c in enumerate(s_c)]
        return {"scores": sc, "total": t_c[0][0] if t_c else None, "bonus": bf.value,
                "status": "flagged", "flag": "mismatch", "raw": primary}

    def _learn(self, fields: List[FieldRead]):
        if self.engine.name != "template":
            return
        for f in fields:
            if f.feats:
                self.engine.templates.learn(f.feats, f.digits)

    @staticmethod
    def stage_crop(screen: Image.Image, cfg: Dict[str, Any], stage: int) -> Image.Image:
        """レビュー画面用：そのステージの合計〜スコア行〜ボーナス行までの切り抜き．"""
        r = cfg["regions"]
        t, sc, b = r["total"][stage], r["score"][stage], r["bonus"][stage]
        y0 = t[1] - 0.005
        y1 = max(sc[1] + sc[3], b[1] + b[3]) + 0.005
        return crop_rel(screen, [0.22, y0, 0.56, y1 - y0])
