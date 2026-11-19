package `fun`.hackerlife.octendra

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.util.Base64
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream

/** Sparse AI observations; Weavelo video must stay on its native encoder path. */
object UiImage {
    internal fun isSolidBlackPixels(pixels: IntArray, width: Int, height: Int): Boolean {
        if (width < 8 || height < 8 || pixels.size < width * height) return false
        for (gy in 1..8) for (gx in 1..8) {
            val px = pixels[(height * gy / 9) * width + (width * gx / 9)]
            if ((px and 0x00FFFFFF) != 0) return false
        }
        return true
    }

    internal fun tileHashesFromPixels(pixels: IntArray, width: Int, height: Int, tileSize: Int = 64): List<Long> {
        if (width <= 0 || height <= 0 || pixels.size < width * height) return emptyList()
        val cols = (width + tileSize - 1) / tileSize
        val rows = (height + tileSize - 1) / tileSize
        val hashes = ArrayList<Long>(cols * rows)
        for (ty in 0 until rows) {
            val y0 = ty * tileSize
            val y1 = minOf(height, y0 + tileSize)
            for (tx in 0 until cols) {
                val x0 = tx * tileSize
                val x1 = minOf(width, x0 + tileSize)
                var hash = -3750763034362895579L // FNV-1a 64-bit offset basis
                var y = y0
                while (y < y1) {
                    val rowOff = y * width
                    var x = x0
                    while (x < x1) {
                        val px = pixels[rowOff + x] and 0x00F8F8F8 // Tolerate minor sub-pixel quantization
                        hash = (hash xor px.toLong()) * 1099511628211L
                        x += 2
                    }
                    y += 2
                }
                hashes.add(hash)
            }
        }
        return hashes
    }

    fun tileHashes(bitmap: Bitmap): List<Long> {
        val w = bitmap.width
        val h = bitmap.height
        if (w <= 0 || h <= 0) return emptyList()
        val pixels = IntArray(w * h)
        bitmap.getPixels(pixels, 0, w, 0, 0, w, h)
        return tileHashesFromPixels(pixels, w, h)
    }

    private fun isSolidBlack(bitmap: Bitmap): Boolean {
        if (bitmap.width < 8 || bitmap.height < 8) return false
        for (gy in 1..8) for (gx in 1..8) {
            val px = bitmap.getPixel(bitmap.width * gx / 9, bitmap.height * gy / 9)
            if ((px and 0x00FFFFFF) != 0) return false
        }
        return true
    }

    fun encode(bitmap: Bitmap): JSONObject {
        var limit = 1280
        while (true) {
            val ratio = minOf(1.0, limit.toDouble() / maxOf(bitmap.width, bitmap.height))
            val scaled = Bitmap.createScaledBitmap(bitmap, maxOf(1, (bitmap.width * ratio).toInt()), maxOf(1, (bitmap.height * ratio).toInt()), true)
            try {
                val blank = isSolidBlack(scaled)
                for (quality in listOf(85, 65, 45)) {
                    val output = ByteArrayOutputStream()
                    demand(scaled.compress(Bitmap.CompressFormat.JPEG, quality, output), "screenshot_encode_failed")
                    // Base64 + semantic tree must fit the existing 240KiB durable receipt.
                    if (output.size() <= 112 * 1024) return obj(
                        "mime_type" to "image/jpeg",
                        "width" to scaled.width,
                        "height" to scaled.height,
                        "blank_or_secure" to if (blank) true else null,
                        "data" to Base64.encodeToString(output.toByteArray(), Base64.NO_WRAP)
                    )
                }
            } finally { if (scaled !== bitmap) scaled.recycle() }
            demand(limit > 160, "ui_image_budget_exceeded"); limit /= 2
        }
    }

    /**
     * Draws non-destructive Set-of-Marks (SoM) numeric badges onto a temporary copy of the encoded
     * screenshot for interactive enabled elements, preserving clean tile hashes captured separately.
     */
    fun annotateObservationImage(image: JSONObject, elements: JSONArray): JSONObject {
        if (image.optBoolean("blank_or_secure", false) || elements.length() == 0) return image
        val data = image.optString("data", "")
        if (data.isBlank()) return image

        data class Badge(val label: String, val left: Int, val top: Int, val right: Int, val bottom: Int)

        val candidates = ArrayList<Badge>(minOf(elements.length(), 64))
        for (i in 0 until elements.length()) {
            if (candidates.size >= 64) break
            val el = elements.optJSONObject(i) ?: continue
            if (!el.optBoolean("enabled", true)) continue
            val isInteractive = el.optBoolean("interactive", false) ||
                el.optBoolean("clickable", false) ||
                el.optBoolean("editable", false) ||
                el.optBoolean("checkable", false) ||
                el.optBoolean("long_clickable", false)
            if (!isInteractive) continue
            val bounds = el.optJSONArray("bounds") ?: continue
            if (bounds.length() != 4) continue
            val l = bounds.optInt(0, 0)
            val t = bounds.optInt(1, 0)
            val r = bounds.optInt(2, 0)
            val b = bounds.optInt(3, 0)
            if (r <= l || b <= t) continue
            val rawRef = el.optString("ref", "")
            val label = rawRef.removePrefix("n").ifBlank { i.toString() }
            candidates.add(Badge(label, l, t, r, b))
        }
        if (candidates.isEmpty()) return image

        val rawBytes = runCatching { Base64.decode(data, Base64.DEFAULT) }.getOrNull() ?: return image
        val decoded = runCatching { BitmapFactory.decodeByteArray(rawBytes, 0, rawBytes.size) }.getOrNull()
            ?: return image
        val annotated = try {
            decoded.copy(Bitmap.Config.ARGB_8888, true)
        } finally {
            decoded.recycle()
        } ?: return image

        return try {
            val w = annotated.width
            val h = annotated.height
            if (w < 24 || h < 16) return image

            val canvas = Canvas(annotated)
            val fillPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                color = Color.argb(228, 15, 23, 42)
                style = Paint.Style.FILL
            }
            val borderPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                color = Color.rgb(56, 189, 248)
                style = Paint.Style.STROKE
                strokeWidth = maxOf(1.5f, w / 360f)
            }
            val textPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                color = Color.WHITE
                textSize = maxOf(14f, minOf(22f, w / 34f))
                isFakeBoldText = true
            }
            val fm = textPaint.fontMetrics
            val padX = textPaint.textSize * 0.35f
            val pillH = (fm.descent - fm.ascent) + textPaint.textSize * 0.28f
            val placed = ArrayList<RectF>(candidates.size)
            var drawn = 0

            for (c in candidates) {
                val textW = textPaint.measureText(c.label)
                val pillW = maxOf(pillH * 1.15f, textW + padX * 2f)
                val maxX = maxOf(0f, w - pillW - 1f)
                val maxY = maxOf(0f, h - pillH - 1f)
                var px = (c.left + 2f).coerceIn(0f, maxX)
                var py = (c.top + 2f).coerceIn(0f, maxY)
                var rect = RectF(px, py, px + pillW, py + pillH)
                if (placed.any { RectF.intersects(it, rect) }) {
                    val altX = (c.right - pillW - 2f).coerceIn(0f, maxX)
                    val altY = (c.bottom - pillH - 2f).coerceIn(0f, maxY)
                    val altRect = RectF(altX, altY, altX + pillW, altY + pillH)
                    if (placed.none { RectF.intersects(it, altRect) }) {
                        px = altX
                        py = altY
                        rect = altRect
                    }
                }
                val radius = pillH * 0.32f
                canvas.drawRoundRect(rect, radius, radius, fillPaint)
                canvas.drawRoundRect(rect, radius, radius, borderPaint)
                val baseline = rect.centerY() - (fm.ascent + fm.descent) / 2f
                canvas.drawText(c.label, px + (pillW - textW) / 2f, baseline, textPaint)
                placed.add(rect)
                drawn++
            }
            if (drawn == 0) return image
            val out = encode(annotated)
            out.put("som_badges", drawn)
            out
        } catch (_: Exception) {
            image
        } finally {
            annotated.recycle()
        }
    }
}
