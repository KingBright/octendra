package `fun`.hackerlife.octendra

import android.graphics.Bitmap
import android.util.Base64
import org.json.JSONObject
import java.io.ByteArrayOutputStream

/** Sparse AI observations; Weavelo video must stay on its native encoder path. */
object UiImage {
    fun encode(bitmap: Bitmap): JSONObject {
        var limit = 1280
        while (true) {
            val ratio = minOf(1.0,limit.toDouble()/maxOf(bitmap.width,bitmap.height))
            val scaled = Bitmap.createScaledBitmap(bitmap,maxOf(1,(bitmap.width*ratio).toInt()),maxOf(1,(bitmap.height*ratio).toInt()),true)
            try {
                for (quality in listOf(85,65,45)) {
                    val output = ByteArrayOutputStream()
                    demand(scaled.compress(Bitmap.CompressFormat.JPEG,quality,output),"screenshot_encode_failed")
                    // Base64 + semantic tree must fit the existing 240KiB durable receipt.
                    if (output.size() <= 112*1024) return obj("mime_type" to "image/jpeg","width" to scaled.width,"height" to scaled.height,
                        "data" to Base64.encodeToString(output.toByteArray(),Base64.NO_WRAP))
                }
            } finally { if (scaled !== bitmap) scaled.recycle() }
            demand(limit > 160,"ui_image_budget_exceeded"); limit /= 2
        }
    }
}
