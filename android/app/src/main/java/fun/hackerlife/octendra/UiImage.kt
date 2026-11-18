package `fun`.hackerlife.octendra

import android.graphics.Bitmap
import android.util.Base64
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
}
