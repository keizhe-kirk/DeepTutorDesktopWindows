"""从参考图生成 DeepTutor 桌面端的圆角方形图标母版。

产出 assets/logo-source.png：1024x1024 RGBA，圆角方形底板，圆角外完全透明。

参考图是「翻开的书 + 手型 + 手写 AI」的线条画，笔触是柔和的
蓝 → 灰绿 → 暖橙 → 柔红 渐变，背景纯白不透明。这里要做三件事：

1. 去白底 → 转透明（按亮度重建 alpha，保留抗锯齿与柔和渐变）
2. 裁到图案的实际边界，居中放进圆角方形底板
3. 底板用与笔触同色系的柔和渐变，保证小尺寸下轮廓依然清晰

⚠️ 线条画直接当图标会有两个问题，这里都做了处理：
   - 图案本身不是正方形（918x812），直接拉伸会变形 → 等比缩放后居中
   - 纯透明底 + 细线条在小尺寸（32px）下会糊成一团 → 加柔和渐变底板撑住轮廓
"""

import os
import sys

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

SRC = sys.argv[1] if len(sys.argv) > 1 else None
DST = os.path.join(ROOT, "assets", "logo-source.png")

SIZE = 1024            # 母版边长
CORNER_RADIUS = 200    # 圆角半径，约占边长 19.5%（接近 Windows 11 / macOS 观感）
PAD_RATIO = 0.10       # 图案四周留白比例（相对底板边长）
SUPERSAMPLE = 4        # 圆角遮罩超采样倍数，消除锯齿

# 笔触是极浅的马卡龙色（实测亮度普遍 200+/255），原样缩小到 32px 会糊成一片。
# 这里做一层克制的加深 + 提饱和：把对比度拉起来让轮廓立得住，
# 但不能过头 —— 参考图的气质是"柔和手绘"，饱和度拉满会变成刺眼的彩虹。
#   - LEVELS_FLOOR: 把亮度压到 [FLOOR,255] 区间再重映射，暗部更实
#   - SAT_BOOST:    绕灰度轴放大色度
# 这两组值是试出来的：FLOOR=108/SAT=1.85 太艳，FLOOR=70/SAT=1.35 兼顾清晰与柔和。
LEVELS_FLOOR = 70
SAT_BOOST = 1.35

# 底板渐变：左上柔蓝 → 中灰绿 → 右下暖橙，呼应笔触配色。
# 底板不能太白 —— 深色任务栏上会"发飘"，也和加深后的笔触对比过强。
BG_START = (233, 239, 245)
BG_END = (250, 237, 227)
BG_MID = (240, 240, 236)


def _saturate(rgb, boost: float):
    """绕灰度轴放大色度。boost>1 更鲜艳，=1 不变。"""
    r, g, b = rgb
    lum = 0.299 * r + 0.587 * g + 0.114 * b
    return (
        max(0, min(255, int(round(lum + (r - lum) * boost)))),
        max(0, min(255, int(round(lum + (g - lum) * boost)))),
        max(0, min(255, int(round(lum + (b - lum) * boost)))),
    )


def _level(rgb, floor: int):
    """把 [floor,255] 线性拉伸到 [0,255]，暗部更实。"""
    span = 255 - floor
    return tuple(max(0, min(255, int(round((c - floor) / span * 255)))) for c in rgb)


def remove_white_background(img: Image.Image) -> Image.Image:
    """把纯白背景转成透明，保留抗锯齿与柔和渐变。

    线条画的抗锯齿边缘是「彩色像素与白色按比例混合」的结果，
    所以直接按亮度做二元阈值会切出毛边。这里用逐像素的
    「离白距离」重建 alpha：越接近白 → 越透明，笔触中心保持不透明。
    """
    img = img.convert("RGBA")
    w, h = img.size
    px = img.load()

    # 以"最大分量接近 255 且三分量接近"判定白色
    out = Image.new("RGBA", (w, h))
    opx = out.load()

    for y in range(h):
        for x in range(w):
            r, g, b, _ = px[x, y]
            # 离白距离：取三通道中最大的缺口，代表该像素"有多少颜色"
            d = max(255 - r, 255 - g, 255 - b)
            if d <= 6:
                # 几乎纯白 → 全透明
                opx[x, y] = (r, g, b, 0)
                continue
            # 映射成 alpha。用 6..80 这段做渐变区，保证边缘平滑
            if d >= 80:
                a = 255
            else:
                a = int(round((d - 6) / (80 - 6) * 255))
            # 提饱和 + 压暗部，让笔触在 32px 下依然立得住
            cr, cg, cb = _level(_saturate((r, g, b), SAT_BOOST), LEVELS_FLOOR)
            opx[x, y] = (cr, cg, cb, a)

    return out


def make_rounded_mask(size: int, radius: int, ss: int = SUPERSAMPLE) -> Image.Image:
    """生成圆角矩形遮罩（超采样后缩小，边缘干净）。"""
    big = size * ss
    brad = radius * ss
    mask = Image.new("L", (big, big), 0)
    d = ImageDraw.Draw(mask)
    d.rounded_rectangle((0, 0, big - 1, big - 1), radius=brad, fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def make_background(size: int, radius: int) -> Image.Image:
    """柔和对角三段渐变底板 + 圆角遮罩。"""
    grad = Image.new("RGBA", (size, size))
    px = grad.load()
    denom = (size - 1) * 2
    for y in range(size):
        for x in range(size):
            t = (x + y) / denom
            # 三段：BG_START → BG_MID(t=0.5) → BG_END
            if t < 0.5:
                u = t / 0.5
                c0, c1 = BG_START, BG_MID
            else:
                u = (t - 0.5) / 0.5
                c0, c1 = BG_MID, BG_END
            r = int(round(c0[0] + (c1[0] - c0[0]) * u))
            g = int(round(c0[1] + (c1[1] - c0[1]) * u))
            b = int(round(c0[2] + (c1[2] - c0[2]) * u))
            px[x, y] = (r, g, b, 255)

    mask = make_rounded_mask(size, radius)
    grad.putalpha(mask)
    return grad


def fit_into_square(img: Image.Image, size: int, pad_ratio: float) -> Image.Image:
    """按图案实际边界裁剪，等比缩放到 (size - 2*pad) 的方框内并居中。"""
    bbox = img.split()[3].getbbox()
    if bbox is None:
        raise SystemExit("图案全透明，去白底可能失败了")
    cropped = img.crop(bbox)

    inner = int(size * (1 - 2 * pad_ratio))
    w, h = cropped.size
    scale = min(inner / w, inner / h)
    new = (max(1, int(round(w * scale))), max(1, int(round(h * scale))))
    resized = cropped.resize(new, Image.LANCZOS)

    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(resized, ((size - new[0]) // 2, (size - new[1]) // 2), resized)
    return canvas


def main() -> None:
    if not SRC:
        raise SystemExit("用法: make-icon.py <参考图路径>")
    if not os.path.isfile(SRC):
        raise SystemExit(f"参考图不存在: {SRC}")

    src = Image.open(SRC)
    print(f"[1/5] 读取参考图: {src.size} {src.mode}")

    transparent = remove_white_background(src)
    bbox = transparent.split()[3].getbbox()
    print(f"[2/5] 去白底完成，图案边界: {bbox}")

    bg = make_background(SIZE, CORNER_RADIUS)
    print(f"[3/5] 圆角底板完成: {SIZE}x{SIZE} radius={CORNER_RADIUS}")

    art = fit_into_square(transparent, SIZE, PAD_RATIO)
    print(f"[4/5] 图案已等比居中，留白比例 {PAD_RATIO:.0%}")

    final = Image.alpha_composite(bg, art)
    os.makedirs(os.path.dirname(DST), exist_ok=True)
    final.save(DST, "PNG")
    print(f"[5/5] 已保存: {DST} ({final.size}, {final.mode})")


if __name__ == "__main__":
    main()
