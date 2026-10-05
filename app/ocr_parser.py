import re
from typing import Dict, Any, List, Optional, Tuple
from PIL import Image, ImageEnhance
import winocr

class RehearsalOcrParser:
    """リハーサル結果画面からスコア表示領域を特定し，総合力や合計スコアの誤認を排除して高精度に抽出するパーサー．"""

    @staticmethod
    def extract_scores_from_text(text: str) -> List[int]:
        """OCR認識結果テキストからスコア群（1万〜1000万）を抽出します．"""
        # +〇〇（王冠ボーナス）を安全に除外
        filtered = re.sub(r"\+\s*\d+", "", text)
        cleaned = re.sub(r"\s*([,\.])\s*", ",", filtered)

        # 3桁区切りのカンマ付き数字
        matched = re.findall(r"\b\d{1,3}(?:,\d{3})+\b", cleaned)
        scores = []
        for m in matched:
            val = int(m.replace(",", ""))
            if 10000 <= val <= 10000000:
                scores.append(val)

        # ピリオド区切りのフォールバック
        if len(scores) < 3:
            matched_dot = re.findall(r"\b\d{1,3}(?:\.\d{3})+\b", filtered)
            for md in matched_dot:
                val = int(md.replace(".", ""))
                if 10000 <= val <= 10000000 and val not in scores:
                    scores.append(val)

        # カンマなし5〜7桁数字のフォールバック
        if len(scores) < 3:
            raw_nums = re.findall(r"\b\d{5,7}\b", cleaned)
            for rn in raw_nums:
                val = int(rn)
                if 10000 <= val <= 10000000 and val not in scores:
                    scores.append(val)

        return scores[:3]

    async def parse_image(self, image: Image.Image) -> Dict[str, Any]:
        """画像から各ステージのスコア行を検出し，総合力や合計スコアを除外して解析します．"""
        w, h = image.size

        # 全体をOCR認識
        res = await winocr.recognize_pil(image, lang="ja")

        # 1行の中に2〜3個のスコア数値が含まれている行（各ステージのスコア表示行）を探索
        candidate_lines: List[Tuple[float, List[int]]] = []
        for line in res.lines:
            nums = self.extract_scores_from_text(line.text)
            if len(nums) >= 2:
                # 行の平均Y座標
                ys = [w_obj.bounding_rect.y for w_obj in line.words if hasattr(w_obj, "bounding_rect")]
                avg_y = sum(ys) / len(ys) if ys else 0.0
                candidate_lines.append((avg_y, nums))

        # Y座標順（上から下）にソート
        candidate_lines.sort(key=lambda x: x[0])

        stages_data = []

        if len(candidate_lines) >= 3:
            # 3ステージすべて綺麗に検出できた場合
            for idx in range(3):
                scores = candidate_lines[idx][1]
                stages_data.append({
                    "stage": idx + 1,
                    "scores": scores,
                    "total_score": sum(scores) if scores else None
                })
        else:
            # 万が一3行揃わなかった場合のフォールバック（ピンポイント領域クロップ＆拡大処理）
            stage_crops = [
                (1, int(w * 0.12), int(h * 0.208), int(w * 0.88), int(h * 0.252)),
                (2, int(w * 0.12), int(h * 0.448), int(w * 0.88), int(h * 0.492)),
                (3, int(w * 0.12), int(h * 0.688), int(w * 0.88), int(h * 0.732)),
            ]
            for stg, x1, y1, x2, y2 in stage_crops:
                matched_scores = []
                for cy, nums in candidate_lines:
                    if y1 - h * 0.05 <= cy <= y2 + h * 0.05:
                        matched_scores = nums
                        break

                if matched_scores:
                    stages_data.append({
                        "stage": stg,
                        "scores": matched_scores,
                        "total_score": sum(matched_scores) if matched_scores else None
                    })
                else:
                    strip = image.crop((x1, y1, x2, y2))
                    sw, sh = strip.size
                    if sw > 0 and sh > 0:
                        scaled = strip.resize((sw * 3, sh * 3), Image.Resampling.LANCZOS)
                        enhancer = ImageEnhance.Contrast(scaled)
                        scaled = enhancer.enhance(1.3)
                        strip_res = await winocr.recognize_pil(scaled, lang="ja")
                        strip_text = " ".join([l.text for l in strip_res.lines])
                        scores = self.extract_scores_from_text(strip_text)
                    else:
                        scores = []

                    stages_data.append({
                        "stage": stg,
                        "scores": scores,
                        "total_score": sum(scores) if scores else None
                    })

        return {
            "success": True,
            "stages": stages_data
        }
