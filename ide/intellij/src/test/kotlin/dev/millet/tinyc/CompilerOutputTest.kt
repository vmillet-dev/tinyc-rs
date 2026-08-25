package dev.millet.tinyc

import dev.millet.tinyc.annotator.TinyCDiagnostics
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit

/**
 * The other half of [TinyCDiagnosticsTest]: the same parsing, run against what
 * the compiler on this machine actually prints rather than against text pasted
 * from a run.
 *
 * The samples in the other test can go stale without anything noticing; this one
 * cannot. It is skipped when no `tinyc` has been built, because a test suite
 * that fails on a fresh clone is a test suite people learn to ignore.
 */
class CompilerOutputTest {

    @Test
    fun `what the compiler refuses is what the editor marks`() {
        val repository = repository() ?: return
        val compiler = compiler(repository) ?: return
        val source = repository.resolve("examples/errors/undeclared_variable.tc")
        assumeTrue("the example is not there", Files.isRegularFile(source))

        val diagnostics = TinyCDiagnostics.parse(run(compiler, source, repository))
        assertEquals("one mistake in the file, one mark in the editor", 1, diagnostics.size)

        val diagnostic = diagnostics.single()
        assertTrue("the message names the variable: ${diagnostic.message}", diagnostic.message.contains("z"))
        assertTrue(diagnostic.line > 0)
        assertTrue(diagnostic.column > 0)
        assertEquals("`z` is one character", 1, diagnostic.length)
    }

    /** Every mistake, not only the first — which is what the front end promises. */
    @Test
    fun `several mistakes come back as several marks`() {
        val repository = repository() ?: return
        val compiler = compiler(repository) ?: return

        val scratch = Files.createTempFile("tinyc-plugin-test-", ".tc")
        try {
            Files.writeString(
                scratch,
                """
                fn main() {
                  println(a);
                  println(b);
                  println(c);
                }
                """.trimIndent(),
            )
            val diagnostics = TinyCDiagnostics.parse(run(compiler, scratch, repository))
            assertEquals(3, diagnostics.size)
            assertEquals(listOf(2, 3, 4), diagnostics.map { it.line })
        } finally {
            Files.deleteIfExists(scratch)
        }
    }

    private fun run(compiler: Path, source: Path, workingDirectory: Path): String {
        // The IR itself is thrown away — only what the compiler refused matters
        // here, and draining one pipe while the other fills is a deadlock.
        val process = ProcessBuilder(compiler.toString(), "--emit", "ir", source.toString())
            .directory(workingDirectory.toFile())
            .redirectOutput(ProcessBuilder.Redirect.DISCARD)
            .start()
        val errors = process.errorStream.readBytes().toString(Charsets.UTF_8)
        assertTrue("the compiler did not finish", process.waitFor(60, TimeUnit.SECONDS))
        return errors
    }

    private fun repository(): Path? {
        val path = System.getProperty("tinyc.repo")?.let { Path.of(it) }
        assumeTrue("the repository is not next to the plugin", path != null && Files.isDirectory(path))
        return path
    }

    private fun compiler(repository: Path): Path? {
        val binary = listOf("release", "debug")
            .map { repository.resolve("target/$it/" + if (isWindows) "tinyc.exe" else "tinyc") }
            .firstOrNull { Files.isExecutable(it) }
        assumeTrue("no tinyc has been built (cargo build --release)", binary != null)
        return binary
    }

    private val isWindows: Boolean
        get() = System.getProperty("os.name").lowercase().contains("win")
}
