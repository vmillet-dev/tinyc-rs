package dev.millet.tinyc

import dev.millet.tinyc.annotator.TinyCDiagnostics
import dev.millet.tinyc.run.TinyCBuild
import dev.millet.tinyc.toolchain.TinyCTooling
import dev.millet.tinyc.toolchain.TinyCTools
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit

/**
 * The plugin's own pipeline, run for real: source, assembly, object,
 * executable, output.
 *
 * This is the test that says the plugin no longer needs the repository's build
 * scripts — it finds the three tools the way the plugin does, and drives them
 * the way the plugin does. It is skipped on a machine that has not got them,
 * because a suite that fails on a fresh clone is one people learn to ignore.
 */
class TinyCBuildTest {

    @Test
    fun `a program is built and runs`() {
        val tools = tools() ?: return
        val repository = repository()!!
        val source = repository.resolve("examples/hello.tc")
        assumeTrue("examples/hello.tc is not there", Files.isRegularFile(source))

        val output = Files.createTempDirectory("tinyc-plugin-build-")
        try {
            val outcome = TinyCBuild.build(tools, source, output, repository)
            assertTrue("the build should succeed:\n${outcome.log}", outcome.built)

            val executable = outcome.executable!!
            assertTrue("$executable should exist", Files.isExecutable(executable))
            assertTrue("the assembly should be kept", Files.isRegularFile(outcome.assembly!!))

            // The log reads like the scripts' does, which is what the console shows.
            assertTrue(outcome.log, outcome.log.contains("==> tinyc"))
            assertTrue(outcome.log, outcome.log.contains("==> nasm"))
            assertTrue(outcome.log, outcome.log.contains("==> built"))

            assertTrue("the program should print its greeting", run(executable).contains("Hello World"))
        } finally {
            output.toFile().deleteRecursively()
        }
    }

    /**
     * A program the compiler refuses stops the pipeline at the first step, and
     * what comes back is the diagnostic — the same text the editor marks with,
     * so the console and the gutter cannot disagree.
     */
    @Test
    fun `a program that does not compile stops at the compiler`() {
        val tools = tools() ?: return
        val repository = repository()!!

        val output = Files.createTempDirectory("tinyc-plugin-build-")
        val source = output.resolve("broken.tc")
        Files.writeString(source, "fn main() {\n  println(nothing);\n}\n")
        try {
            val outcome = TinyCBuild.build(tools, source, output, repository)
            assertFalse("the build should fail:\n${outcome.log}", outcome.built)
            assertTrue(outcome.log, outcome.log.contains("==> compiling failed"))
            assertFalse("nothing was assembled", outcome.log.contains("==> nasm"))

            val diagnostics = TinyCDiagnostics.parse(outcome.log)
            assertEquals(1, diagnostics.size)
            assertEquals(2, diagnostics.single().line)
        } finally {
            output.toFile().deleteRecursively()
        }
    }

    /** What the compiler is handed can be steered from the run configuration. */
    @Test
    fun `compiler arguments reach the compiler`() {
        val tools = tools() ?: return
        val repository = repository()!!
        val source = repository.resolve("examples/arith.tc")
        assumeTrue("examples/arith.tc is not there", Files.isRegularFile(source))

        val output = Files.createTempDirectory("tinyc-plugin-build-")
        try {
            val optimised = TinyCBuild.build(tools, source, output.resolve("on"), repository)
            val plain = TinyCBuild.build(tools, source, output.resolve("off"), repository, listOf("--no-optimise"))
            assertTrue(optimised.log, optimised.built)
            assertTrue(plain.log, plain.built)

            // The optimiser is what makes the difference visible: the same
            // program, fewer instructions.
            val with = Files.readString(optimised.assembly!!).lines().size
            val without = Files.readString(plain.assembly!!).lines().size
            assertTrue("--no-optimise should produce more assembly ($without vs $with)", without > with)
        } finally {
            output.toFile().deleteRecursively()
        }
    }

    private fun run(executable: Path): String {
        val process = ProcessBuilder(executable.toString())
            .redirectErrorStream(true)
            .start()
        process.outputStream.close()
        val printed = process.inputStream.readBytes().toString(Charsets.UTF_8)
        assertTrue("the program did not finish", process.waitFor(60, TimeUnit.SECONDS))
        return printed
    }

    private fun repository(): Path? {
        val path = System.getProperty("tinyc.repo")?.let { Path.of(it) }
        assumeTrue("the repository is not next to the plugin", path != null && Files.isDirectory(path))
        return path
    }

    /** The three tools, found exactly as the plugin finds them with no settings. */
    private fun tools(): TinyCTools? {
        val repository = repository() ?: return null
        val near = repository.resolve("examples/hello.tc")
        val compiler = TinyCTooling.findCompiler(null, near)
        val nasm = TinyCTooling.findNasm(null)
        val linker = TinyCTooling.findLinker(null, null)
        assumeTrue("no tinyc (cargo build --release)", compiler != null)
        assumeTrue("no nasm on this machine", nasm != null)
        assumeTrue("no linker on this machine", linker != null)
        return TinyCTools(compiler!!, nasm!!, linker!!)
    }
}
