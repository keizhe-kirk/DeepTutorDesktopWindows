"""处理 logo:裁掉底部 AI 水印后 resize 回 1024x1024 RGBA。"""
from PIL import Image

SRC = r"D:\code\DeepTutorDesktopWin\assets\Modern_flat_app_icon_for__Deep_2026-09-09T06-08-39.png"
DST = r"D:\code\DeepTutorDesktopWin\assets\logo-source.png"

img = Image.open(SRC).convert("RGBA")
w, h = img.size
print(f"source: {w}x{h}")

# 裁掉底部 8% (水印在底部右下角)
crop_pct = 0.08
crop_h = int(h * crop_pct)
cropped = img.crop((0, 0, w, h - crop_h))
print(f"after crop: {cropped.size}")

# resize 回 1024x1024 (轻微拉伸, 8% 视觉几乎不可见)
final = cropped.resize((1024, 1024), Image.LANCZOS)
final.save(DST, "PNG")
print(f"saved: {DST} ({final.size}, mode={final.mode})")