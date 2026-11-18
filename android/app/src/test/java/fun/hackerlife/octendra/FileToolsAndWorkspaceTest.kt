package `fun`.hackerlife.octendra

import org.json.JSONArray
import org.junit.Assert.*
import org.junit.Test
import java.io.File
import java.nio.file.Files

class FileToolsAndWorkspaceTest {
    @Test fun defaultWorkspaceAliasesResolveOnlyToAuthorizedRoot() {
        val root = Files.createTempDirectory("octendra-ws-test").toFile().canonicalFile
        try {
            val dev = "123e4567-e89b-42d3-a456-426614174000"
            assertEquals("$dev:default", WorkspaceResolution.canonicalDefaultId(dev, root, "default"))
            assertEquals("$dev:default", WorkspaceResolution.canonicalDefaultId(dev, root, ":default"))
            assertEquals("$dev:default", WorkspaceResolution.canonicalDefaultId(dev, root, "$dev:default"))
            assertEquals("$dev:${root.path}", WorkspaceResolution.canonicalDefaultId(dev, root, "$dev:${root.path}"))
            assertNull(WorkspaceResolution.canonicalDefaultId(dev, root, "$dev:/etc"))
            assertNull(WorkspaceResolution.canonicalDefaultId(dev, root, "$dev:random-uuid"))
        } finally {
            root.deleteRecursively()
        }
    }

    @Test fun applyEditsSupportsCreateEditPatchDeleteAndOptimisticLock() {
        val root = Files.createTempDirectory("octendra-edits-test").toFile().canonicalFile
        try {
            // 1. Create file with expected_version = absent
            val createRes = FileTools.applyEdits(
                root,
                obj(
                    "files" to JSONArray(
                        listOf(
                            obj(
                                "path" to "src/hello.txt",
                                "action" to "create",
                                "expected_version" to "absent",
                                "content" to "line1\nline2\nline3\n"
                            )
                        )
                    )
                ),
                "op-1"
            )
            assertEquals("completed", createRes.getString("state"))
            val f = File(root, "src/hello.txt")
            val v1 = createRes.getJSONArray("changed").getJSONObject(0).getString("version")
            assertEquals(FileTools.digest(f), v1)

            // 2. Edit with wrong expected_version must fail with version_conflict without touching disk
            try {
                FileTools.applyEdits(
                    root,
                    obj(
                        "files" to JSONArray(
                            listOf(
                                obj(
                                    "path" to "src/hello.txt",
                                    "expected_version" to "0".repeat(64),
                                    "edits" to JSONArray(listOf(obj("old_text" to "line2", "new_text" to "bad")))
                                )
                            )
                        )
                    ),
                    "op-2"
                )
                fail("version conflict must reject edit")
            } catch (e: AgentError) {
                assertEquals("version_conflict", e.code)
                assertEquals("line1\nline2\nline3\n", f.readText())
            }

            // 3. Edit with matching SHA-256 expected_version replaces atomically
            val editRes = FileTools.applyEdits(
                root,
                obj(
                    "files" to JSONArray(
                        listOf(
                            obj(
                                "path" to "src/hello.txt",
                                "expected_version" to v1,
                                "edits" to JSONArray(listOf(obj("old_text" to "line2", "new_text" to "updated2")))
                            )
                        )
                    )
                ),
                "op-3"
            )
            val v2 = editRes.getJSONArray("changed").getJSONObject(0).getString("version")
            assertEquals("line1\nupdated2\nline3\n", f.readText())
            assertEquals(FileTools.digest(f), v2)

            // 4. Unified patch edit
            val patchRes = FileTools.applyEdits(
                root,
                obj(
                    "files" to JSONArray(
                        listOf(
                            obj(
                                "path" to "src/hello.txt",
                                "expected_version" to v2,
                                "patch" to "@@ -1,3 +1,3 @@\n line1\n-updated2\n+patched2\n line3"
                            )
                        )
                    )
                ),
                "op-4"
            )
            assertEquals("line1\npatched2\nline3\n", f.readText())
            val v3 = patchRes.getJSONArray("changed").getJSONObject(0).getString("version")

            // 5. Delete via code_apply_edits
            val delRes = FileTools.applyEdits(
                root,
                obj("files" to JSONArray(listOf(obj("path" to "src/hello.txt", "action" to "delete", "expected_version" to v3)))),
                "op-5"
            )
            assertEquals("absent", delRes.getJSONArray("changed").getJSONObject(0).getString("version"))
            assertFalse(f.exists())
        } finally {
            root.deleteRecursively()
        }
    }

    @Test fun fileDeleteEnforcesWorkspaceBoundsAndOptimisticLock() {
        val root = Files.createTempDirectory("octendra-del-test").toFile().canonicalFile
        val outside = Files.createTempDirectory("octendra-outside-del").toFile().canonicalFile
        try {
            val target = File(root, "notes/todo.txt").apply { parentFile!!.mkdirs(); writeText("keep") }
            val secret = File(outside, "secret.txt").apply { writeText("secret") }
            Files.createSymbolicLink(root.toPath().resolve("link"), outside.toPath())

            val sha = FileTools.digest(target)
            // Wrong SHA-256 refuses deletion
            try {
                FileTools.deleteFile(root, obj("path" to "notes/todo.txt", "expected_version" to "1".repeat(64)))
                fail("version mismatch must reject delete")
            } catch (e: AgentError) {
                assertEquals("version_conflict", e.code)
                assertTrue(target.exists())
            }

            // Symlink and parent escape rejected
            for (bad in listOf("../secret.txt", "/etc/passwd", "link/secret.txt")) {
                try {
                    FileTools.deleteFile(root, obj("path" to bad))
                    fail("unsafe path accepted: $bad")
                } catch (_: AgentError) { }
            }
            assertTrue(secret.exists())

            // Valid SHA-256 deletes file
            val res = FileTools.deleteFile(root, obj("path" to "notes/todo.txt", "expected_version" to sha))
            assertTrue(res.getBoolean("deleted"))
            assertEquals("absent", res.getString("version"))
            assertFalse(target.exists())
        } finally {
            root.deleteRecursively()
            outside.deleteRecursively()
        }
    }
}
