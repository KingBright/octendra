package `fun`.hackerlife.octendra

import android.content.Context
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.InputStream
import java.nio.file.AtomicMoveNotSupportedException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest

class FileTools(private val context: Context, private val transport: Transport) {
    private fun limit(args: JSONObject): Long = args.optLong("max_bytes", Policy.MAX_FILE).also { demand(it in 1..Policy.MAX_FILE, "file_limit_exceeded") }
    companion object {
        private val publicationLock = Any()

        private fun atomicReplace(temporary: File, destination: File) {
            try {
                Files.move(
                    temporary.toPath(),
                    destination.toPath(),
                    StandardCopyOption.REPLACE_EXISTING,
                    StandardCopyOption.ATOMIC_MOVE
                )
            } catch (_: AtomicMoveNotSupportedException) {
                Files.move(temporary.toPath(), destination.toPath(), StandardCopyOption.REPLACE_EXISTING)
            }
        }

        /** All app writers share this lock; Android app-private data has one UID.
         * Publish by atomic rename, rechecking the expected version under the same writer lock. */
        fun publish(root: File, path: String, temporary: File, expected: String) = synchronized(publicationLock) {
            val destination = Policy.within(root, path)
            demand(temporary.isFile && temporary.canonicalFile == temporary.absoluteFile, "invalid_staged_file")
            demand(version(destination) == expected, "version_conflict")
            atomicReplace(temporary, destination)
        }

        fun digest(file: File): String {
            val h = MessageDigest.getInstance("SHA-256")
            file.inputStream().use { input -> val b = ByteArray(65536); while (true) { val n = input.read(b); if (n < 0) break; h.update(b, 0, n) } }
            return h.digest().joinToString("") { "%02x".format(it) }
        }

        fun copy(input: InputStream, output: FileOutputStream, max: Long): Long {
            val buffer = ByteArray(65536); var count = 0L
            while (true) { if (Thread.currentThread().isInterrupted) throw InterruptedException(); val n = input.read(buffer); if (n < 0) break; count += n; demand(count <= max, "file_limit_exceeded"); output.write(buffer, 0, n) }
            output.fd.sync(); return count
        }

        fun version(file: File): String = if (!file.exists()) "absent" else {
            demand(!Files.isSymbolicLink(file.toPath()) && file.isFile && file.length() <= Policy.MAX_FILE, "invalid_existing_file")
            digest(file)
        }

        fun deleteFile(root: File, args: JSONObject): JSONObject = synchronized(publicationLock) {
            val path = args.getString("path")
            val file = Policy.within(root, path)
            demand(file.exists(), "file_not_found")
            demand(!Files.isSymbolicLink(file.toPath()) && file.isFile, "invalid_existing_file")
            val currentVersion = version(file)
            if (args.has("expected_version") && !args.isNull("expected_version")) {
                val expected = args.getString("expected_version")
                demand(expected == "absent" || expected.matches(Regex("[0-9a-f]{64}")), "invalid_expected_version")
                demand(currentVersion == expected, "version_conflict")
            }
            demand(file.delete(), "delete_failed")
            obj("path" to path, "deleted" to true, "before_version" to currentVersion, "version" to "absent", "state" to "completed")
        }

        private data class PreparedEdit(
            val path: String,
            val target: File,
            val action: String,
            val beforeVersion: String,
            val newBytes: ByteArray?,
            val newVersion: String
        )

        private fun transformText(oldText: String, entry: JSONObject): String {
            val hasContent = entry.has("content") && !entry.isNull("content")
            val hasPatch = entry.has("patch") && !entry.isNull("patch")
            val hasEdits = entry.has("edits") && !entry.isNull("edits")
            if (hasContent) {
                demand(!hasEdits && !hasPatch, "invalid_edit_arguments")
                return entry.getString("content")
            }
            if (hasPatch) {
                demand(!hasEdits, "invalid_edit_arguments")
                return applyUnifiedPatch(oldText, entry.getString("patch"))
            }
            demand(hasEdits, "missing_edits_or_patch")
            val edits = entry.getJSONArray("edits")
            demand(edits.length() in 1..100, "edit_count_exceeded")
            val ranges = ArrayList<Triple<Int, Int, String>>(edits.length())
            for (i in 0 until edits.length()) {
                val rep = edits.getJSONObject(i)
                val oldPart = rep.getString("old_text")
                val newPart = rep.getString("new_text")
                demand(oldPart.isNotEmpty(), "empty_old_text")
                val positions = mutableListOf<Int>()
                var from = 0
                while (true) {
                    val idx = oldText.indexOf(oldPart, from)
                    if (idx < 0) break
                    positions.add(idx)
                    from = idx + maxOf(1, oldPart.length)
                }
                demand(positions.size == 1, "match_conflict")
                ranges.add(Triple(positions[0], positions[0] + oldPart.length, newPart))
            }
            ranges.sortBy { it.first }
            for (i in 1 until ranges.size) {
                demand(ranges[i - 1].second <= ranges[i].first, "overlapping_edits")
            }
            val sb = StringBuilder(oldText)
            for (i in ranges.indices.reversed()) {
                val (start, end, replacement) = ranges[i]
                sb.replace(start, end, replacement)
            }
            return sb.toString()
        }

        private fun applyUnifiedPatch(oldText: String, patch: String): String {
            val patchLines = patch.split('\n')
            val hadTrailingNewline = oldText.endsWith("\n")
            val oldLines = if (oldText.isEmpty()) emptyList()
            else oldText.split('\n').let { if (hadTrailingNewline) it.dropLast(1) else it }
            val result = ArrayList<String>()
            var oldIdx = 0
            var i = 0
            while (i < patchLines.size && !patchLines[i].startsWith("@@ ")) i++
            demand(i < patchLines.size, "patch_conflict")
            val hunkHeader = Regex("^@@ -(\\d+)(?:,(\\d+))? \\+(\\d+)(?:,(\\d+))? @@.*$")
            while (i < patchLines.size) {
                val line = patchLines[i]
                if (!line.startsWith("@@ ")) { i++; continue }
                val m = hunkHeader.matchEntire(line) ?: throw AgentError("patch_conflict")
                val oldStart = (m.groupValues[1].toIntOrNull() ?: throw AgentError("patch_conflict"))
                val targetStartIdx = if (oldStart == 0) 0 else oldStart - 1
                demand(targetStartIdx in oldIdx..oldLines.size, "patch_conflict")
                while (oldIdx < targetStartIdx) {
                    result.add(oldLines[oldIdx++])
                }
                i++
                while (i < patchLines.size && !patchLines[i].startsWith("@@ ")) {
                    val hLine = patchLines[i]
                    if (hLine.startsWith("\\")) { i++; continue }
                    if (hLine.isEmpty()) { i++; continue }
                    val prefix = hLine[0]
                    val body = hLine.substring(1)
                    when (prefix) {
                        ' ' -> {
                            demand(oldIdx < oldLines.size && oldLines[oldIdx] == body, "patch_conflict")
                            result.add(oldLines[oldIdx++])
                        }
                        '-' -> {
                            demand(oldIdx < oldLines.size && oldLines[oldIdx] == body, "patch_conflict")
                            oldIdx++
                        }
                        '+' -> result.add(body)
                        else -> throw AgentError("patch_conflict")
                    }
                    i++
                }
            }
            while (oldIdx < oldLines.size) {
                result.add(oldLines[oldIdx++])
            }
            if (result.isEmpty()) return ""
            val joined = result.joinToString("\n")
            return if (hadTrailingNewline || patch.contains("\n+")) "$joined\n" else joined
        }

        fun applyEdits(root: File, args: JSONObject, id: String): JSONObject = synchronized(publicationLock) {
            val filesArr = args.getJSONArray("files")
            demand(filesArr.length() in 1..20, "edit_file_count_exceeded")
            val allowedFields = setOf("path", "expected_version", "action", "edits", "patch", "content")
            val seenPaths = HashSet<String>()
            val prepared = ArrayList<PreparedEdit>(filesArr.length())
            var totalBytes = 0L

            for (i in 0 until filesArr.length()) {
                val entry = filesArr.getJSONObject(i)
                demand(entry.keys().asSequence().all { it in allowedFields }, "invalid_edit_arguments")
                val path = entry.getString("path")
                demand(seenPaths.add(path), "duplicate_file")
                val target = Policy.within(root, path)
                val exists = target.exists()
                val oldBytes: ByteArray? = if (exists) {
                    demand(!Files.isSymbolicLink(target.toPath()) && target.isFile, "invalid_existing_file")
                    demand(target.length() <= Policy.MAX_FILE, "file_limit_exceeded")
                    val bytes = target.readBytes()
                    demand(!bytes.contains(0.toByte()), "binary_content_unsupported")
                    bytes
                } else null
                val beforeVersion = if (oldBytes == null) "absent" else sha256(oldBytes)
                val hasExpected = entry.has("expected_version") && !entry.isNull("expected_version")
                if (hasExpected) {
                    val expected = entry.getString("expected_version")
                    demand(expected == "absent" || expected.matches(Regex("[0-9a-f]{64}")), "invalid_expected_version")
                    demand(beforeVersion == expected, "version_conflict")
                }
                val action = if (entry.has("action") && !entry.isNull("action")) entry.getString("action") else "edit"
                val newText: String? = when (action) {
                    "create" -> {
                        demand(oldBytes == null, "file_already_exists")
                        demand(entry.isNull("edits") && entry.isNull("patch"), "invalid_create_edit")
                        demand(entry.has("content") && !entry.isNull("content"), "missing_content")
                        entry.getString("content")
                    }
                    "delete" -> {
                        demand(oldBytes != null, "file_not_found")
                        demand(entry.isNull("edits") && entry.isNull("patch") && entry.isNull("content"), "invalid_delete_edit")
                        null
                    }
                    "edit" -> {
                        val hasContent = entry.has("content") && !entry.isNull("content")
                        val oldText = if (hasContent && !hasExpected) {
                            if (oldBytes != null) String(oldBytes, Charsets.UTF_8) else ""
                        } else {
                            demand(oldBytes != null, "file_not_found")
                            String(oldBytes!!, Charsets.UTF_8)
                        }
                        transformText(oldText, entry)
                    }
                    else -> throw AgentError("unsupported_edit_action")
                }
                val newBytes = newText?.toByteArray(Charsets.UTF_8)
                if (newBytes != null) {
                    demand(newBytes.size <= Policy.MAX_FILE, "file_limit_exceeded")
                    demand(!newBytes.contains(0.toByte()), "binary_content_unsupported")
                    totalBytes += newBytes.size
                }
                val newVersion = if (newBytes == null) "absent" else sha256(newBytes)
                prepared.add(PreparedEdit(path, target, action, beforeVersion, newBytes, newVersion))
            }

            if (totalBytes > 0L) {
                demand(root.usableSpace > totalBytes + 64L * 1024 * 1024, "storage_reserve_insufficient")
            }

            val stagedTemps = ArrayList<Pair<PreparedEdit, File?>>(prepared.size)
            try {
                for ((idx, item) in prepared.withIndex()) {
                    if (item.newBytes == null) {
                        stagedTemps.add(item to null)
                        continue
                    }
                    item.target.parentFile!!.mkdirs()
                    demand(Policy.within(root, item.path) == item.target, "path_outside_workspace")
                    val tmp = File(item.target.parentFile, ".octendra-edit-$id-$idx.tmp")
                    demand(tmp.createNewFile(), "staging_conflict")
                    stagedTemps.add(item to tmp)
                    FileOutputStream(tmp).use { out ->
                        out.write(item.newBytes)
                        out.fd.sync()
                    }
                }
                // Re-verify all target versions before performing replacements.
                for ((item, _) in stagedTemps) {
                    demand(Policy.within(root, item.path) == item.target, "path_outside_workspace")
                    demand(version(item.target) == item.beforeVersion, "version_conflict")
                }
                val changed = ArrayList<JSONObject>(stagedTemps.size)
                for ((item, tmp) in stagedTemps) {
                    if (tmp != null) {
                        atomicReplace(tmp, item.target)
                    } else {
                        demand(item.target.delete(), "delete_failed")
                    }
                    changed.add(
                        obj(
                            "path" to item.path,
                            "action" to item.action,
                            "before_version" to item.beforeVersion,
                            "version" to item.newVersion
                        )
                    )
                }
                return obj(
                    "changed" to arr(changed),
                    "atomicity" to "per_file",
                    "journal_id" to id,
                    "state" to "completed"
                )
            } finally {
                for ((_, tmp) in stagedTemps) {
                    tmp?.delete()
                }
            }
        }
    }

    fun delete(root: File, args: JSONObject): JSONObject = deleteFile(root, args)

    fun applyEdits(root: File, args: JSONObject, id: String): JSONObject = FileTools.applyEdits(root, args, id)

    fun upload(root: File, args: JSONObject, id: String): JSONObject {
        val path = args.getString("path"); val file = Policy.within(root, path)
        val expected = args.optString("expected_version", "absent")
        demand(expected == "absent" || expected.matches(Regex("[0-9a-f]{64}")), "invalid_expected_version")
        demand(version(file) == expected, "version_conflict")
        val max = limit(args); demand(root.usableSpace > max + 64L * 1024 * 1024, "storage_reserve_insufficient")
        val source = transport.json("/device/file-source/$id")
        val url = Policy.source(source.getString("download_url"), transport.config.gatewayUrl)
        file.parentFile!!.mkdirs(); Policy.within(root, path)
        val tmp = File(file.parentFile, ".octendra-upload-$id.tmp"); demand(tmp.createNewFile(), "staging_conflict")
        val c = transport.connection(url.toString(), "GET", false)
        try {
            demand(c.responseCode == 200, "file_source_rejected")
            val declared = c.contentLengthLong; demand(declared <= max, "file_limit_exceeded")
            val size = c.inputStream.use { input -> FileOutputStream(tmp).use { copy(input, it, max) } }
            if (declared >= 0) demand(declared == size, "incomplete_file")
            val sha = digest(tmp)
            if (args.has("sha256")) demand(args.getString("sha256") == sha, "sha256_mismatch")
            demand(Policy.within(root, path) == file && version(file) == expected, "version_conflict")
            publish(root, path, tmp, expected)
            return obj("path" to path, "size" to size, "sha256" to sha, "version" to sha, "state" to "completed", "atomic" to true)
        } finally { transport.release(c); tmp.delete() }
    }

    fun download(root: File, args: JSONObject, id: String): JSONObject {
        val path = args.getString("path"); val source = Policy.within(root, path); val max = limit(args)
        demand(source.isFile && source.length() <= max, "file_unavailable_or_too_large")
        demand(context.cacheDir.usableSpace >= source.length() + 64L * 1024 * 1024, "storage_reserve_insufficient")
        val snapshot = File.createTempFile("octendra-download-", ".tmp", context.cacheDir)
        try {
            source.inputStream().use { input -> FileOutputStream(snapshot).use { copy(input, it, max) } }
            val sha = digest(snapshot); val size = snapshot.length()
            if (args.has("expected_version")) demand(args.getString("expected_version") == sha, "version_conflict")
            val c = transport.connection(transport.config.gatewayUrl + "/device/files/$id", "POST")
            try {
                c.doOutput = true; c.setFixedLengthStreamingMode(size)
                c.setRequestProperty("Content-Type", "application/octet-stream")
                c.setRequestProperty("x-file-size", size.toString()); c.setRequestProperty("x-file-sha256", sha)
                c.outputStream.use { out -> snapshot.inputStream().use { it.copyTo(out, 65536) } }
                demand(c.responseCode in 200..299, "gateway_file_rejected")
                val receipt = JSONObject(String(c.inputStream.use { Transport.bounded(it, 16384) }, Charsets.UTF_8))
                demand(receipt.getString("sha256") == sha && receipt.getLong("size") == size, "gateway_file_receipt_mismatch")
            } finally { transport.release(c) }
            return obj("artifact_id" to id, "path" to path, "file_name" to source.name, "size" to size, "sha256" to sha, "state" to "completed")
        } finally { snapshot.delete() }
    }

    fun list(root: File, args: JSONObject): JSONObject {
        val glob = args.optString("glob", "**/*")
        demand(glob.length <= 512 && !glob.contains(".."), "invalid_glob")
        val regex = glob.replace(".", "\\.").replace("**/", "\u0001").replace("**", "\u0002").replace("*", "[^/]*").replace("?", "[^/]").replace("\u0001", "(.*/)?").replace("\u0002", ".*").toRegex()
        val files = mutableListOf<String>(); var scanned = 0; val limit = args.optInt("limit", 100).coerceIn(1, 500)
        for (f in root.walkTopDown().maxDepth(12).onEnter { it.canonicalFile == it.absoluteFile }) {
            if (++scanned > 10000) break
            if (f.isFile && f.canonicalFile == f.absoluteFile) {
                val p = f.relativeTo(root).invariantSeparatorsPath
                if (regex.matches(p)) { files += p; if (files.size == limit) break }
            }
        }
        return obj("files" to arr(files), "truncated" to (files.size == limit || scanned > 10000), "next_cursor" to null)
    }

    fun read(root: File, args: JSONObject): JSONObject {
        val requests = args.getJSONArray("requests"); demand(requests.length() in 1..20, "request_count_exceeded")
        var budget = args.optInt("max_bytes", 32768).coerceIn(1024, 65536)
        val ranges = mutableListOf<JSONObject>()
        for (i in 0 until requests.length()) {
            val q = requests.getJSONObject(i); val path = q.getString("path"); val f = Policy.within(root, path)
            demand(f.isFile && f.length() <= 1024 * 1024, "text_file_unavailable_or_too_large")
            val version = digest(f); if (q.has("expected_version")) demand(q.getString("expected_version") == version, "version_conflict")
            val lines = f.readText().split('\n'); val start = q.getInt("start_line"); val end = q.getInt("end_line")
            demand(start >= 1 && end >= start && q.optInt("line_byte_offset", 0) == 0, "invalid_range")
            val picked = mutableListOf<String>(); var index = start - 1
            while (index < lines.size && index < end) { val text = lines[index] + "\n"; if (text.toByteArray().size > budget) break; budget -= text.toByteArray().size; picked += text; index++ }
            val more = index < minOf(end, lines.size)
            ranges += obj("path" to path, "version" to version, "start_line" to start, "end_line" to index, "text" to picked.joinToString(""), "total_lines" to lines.size, "truncated" to more, "next_line" to if (more) index + 1 else null)
        }
        return obj("ranges" to arr(ranges))
    }

    fun pruneScreenshots(root: File) {
        val dir = File(root, "screenshots"); var size = 0L; var count = 0
        dir.listFiles()?.filter { it.name.matches(Regex("[0-9a-f-]{36}\\.png")) && it.isFile && it.canonicalFile == it.absoluteFile }
            ?.sortedByDescending { it.lastModified() }?.forEach {
                count++; size += it.length()
                // Keep fresh captures even during a burst, but refuse new captures at the hard cap in Dispatcher.
                if (it.lastModified() < System.currentTimeMillis() - 3600000 && (count > 24 || size > 64L * 1024 * 1024 || it.lastModified() < System.currentTimeMillis() - 86400000)) it.delete()
            }
    }
}
