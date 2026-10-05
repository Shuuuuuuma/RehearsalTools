import math
import numpy as np
import pandas as pd
from typing import List, Dict, Any, Optional, Tuple


def _nice_width(raw: float) -> int:
    """1・2・2.5・5 × 10^n のきりのいい幅に丸める．"""
    if raw <= 0:
        return 1000
    e = 10 ** math.floor(math.log10(raw))
    for m in (1, 2, 2.5, 5, 10):
        if raw <= m * e:
            return max(1, int(round(m * e)))
    return int(10 * e)


def auto_bins(arr: np.ndarray, min_bins: int = 5, max_bins: int = 12) -> Tuple[int, int, int]:
    """データに合わせて（基準値, 分布幅, 中間の段数）を決める．

    幅は Freedman–Diaconis 法（2×IQR÷n^(1/3)）を元に，ビンの数が 5〜12 個に収まるよう調整し，
    きりのいい数に丸める．最初のビン「〜基準値」に最小値が，最後のビン「〜以上」に最大値が入るようにする．
    """
    lo, hi = float(arr.min()), float(arr.max())
    span = hi - lo
    if span <= 0:
        w = _nice_width(max(1.0, abs(hi) * 0.02))
    else:
        iqr = float(np.percentile(arr, 75) - np.percentile(arr, 25))
        raw = 2 * iqr / (len(arr) ** (1 / 3)) if iqr > 0 else span / 6
        raw = min(max(raw, span / max_bins), span / min_bins)
        w = _nice_width(raw)
        while span / w > max_bins:   # 丸めでビンが多すぎたら一段広げる
            w = _nice_width(w * 1.01)
    base = int(math.floor(lo / w) * w + w)
    steps = max(1, int(math.ceil((hi - base) / w)) - 1) if hi > base else 1
    return base, int(w), steps

class RehearsalAnalytics:
    """リハーサルスコアの統計量および度数分布（ヒストグラム）を算出するクラス．"""

    @staticmethod
    def calculate_statistics(
        scores: List[int],
        target_name: str = "全体",
        base_value: int = 300000,
        bin_width: int = 100000,
        auto: bool = False
    ) -> Dict[str, Any]:
        """画像2枚目の仕様に完全準拠した基本統計量および度数分布表を算出します．"""
        if not scores:
            return {
                "summary": {
                    "target_name": target_name,
                    "mean": 0,
                    "median": 0,
                    "min": 0,
                    "max": 0,
                    "q1": 0,
                    "q3": 0,
                    "count": 0,
                },
                "distribution": {
                    "base_value": base_value,
                    "bin_width": bin_width,
                    "bins": [],
                    "labels": [],
                    "counts": [],
                    "percentages": [],
                }
            }

        arr = np.array(scores, dtype=np.float64)
        n = len(arr)

        # 基本統計量
        mean_val = float(np.mean(arr))
        median_val = float(np.median(arr))
        min_val = int(np.min(arr))
        max_val = int(np.max(arr))
        q1_val = float(np.percentile(arr, 25))
        q3_val = float(np.percentile(arr, 75))

        # 度数分布（ヒストグラム）の算出
        num_steps = 5
        if auto:
            base_value, bin_width, num_steps = auto_bins(arr)
        labels = []
        bin_thresholds = []

        # 最初のビン: ~base_value
        labels.append(f"~{base_value:,}")
        bin_thresholds.append(base_value)

        # 中間ビン
        for i in range(1, num_steps + 1):
            val = base_value + i * bin_width
            labels.append(f"~{val:,}")
            bin_thresholds.append(val)

        # 最後のビン: ~以上
        last_val = bin_thresholds[-1]
        labels.append(f"{last_val:,}以上")

        counts = [0] * len(labels)

        for s in arr:
            if s <= bin_thresholds[0]:
                counts[0] += 1
            elif s > bin_thresholds[-1]:
                counts[-1] += 1
            else:
                for idx in range(1, len(bin_thresholds)):
                    if bin_thresholds[idx - 1] < s <= bin_thresholds[idx]:
                        counts[idx] += 1
                        break

        percentages = [round((c / n) * 100, 1) for c in counts]

        bins_data = []
        for l, c, p in zip(labels, counts, percentages):
            bins_data.append({
                "label": l,
                "count": c,
                "percentage": p
            })

        return {
            "summary": {
                "target_name": target_name,
                "mean": round(mean_val, 2),
                "median": round(median_val, 1),
                "min": min_val,
                "max": max_val,
                "q1": round(q1_val, 2),
                "q3": round(q3_val, 2),
                "count": n,
            },
            "distribution": {
                "base_value": base_value,
                "bin_width": bin_width,
                "auto": auto,
                "bins": bins_data,
                "labels": labels,
                "counts": counts,
                "percentages": percentages,
            }
        }
