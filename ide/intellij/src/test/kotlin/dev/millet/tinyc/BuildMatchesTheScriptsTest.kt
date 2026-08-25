package dev.millet.tinyc

import dev.millet.tinyc.run.TinyCBuild
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.nio.file.Files
import java.nio.file.Path

/**
 * The plugin builds a program itself rather than calling `scripts/build.*`, so
 * that it needs no repository — and that means the pipeline is written twice.
 *
 * This is what keeps the second copy honest: the flags and the libraries are
 * read back out of the scripts and compared with the constants the plugin uses.
 * A change to how a TinyC program is linked that reaches only one of the two is
 * a failing test rather than an executable that behaves differently depending
 * on who built it.
 */
class BuildMatchesTheScriptsTest {

    @Test
    fun `the windows link line is the one the plugin uses`() {
        val script = read("scripts/build.ps1") ?: return

        val line = script.lineSequence().firstOrNull { it.contains("link /nologo") }
        assertTrue("no link line in build.ps1; this test has to be taught the new shape", line != null)

        val words = line!!.split(' ', '`', '"').map { it.trim() }.filter { it.isNotEmpty() }
        val flags = words.filter { it.startsWith("/") && !it.startsWith("/out:") }
        val libraries = words.filter { it.endsWith(".lib") }

        assertEquals(TinyCBuild.MSVC_ARGUMENTS.toSortedSet(), flags.toSortedSet())
        assertEquals(TinyCBuild.MSVC_LIBRARIES.toSortedSet(), libraries.toSortedSet())
    }

    @Test
    fun `the linux link line is the one the plugin uses`() {
        val script = read("scripts/build.sh") ?: return

        val line = script.lineSequence().firstOrNull { it.contains("-no-pie") && it.contains("-o") }
        assertTrue("no link line in build.sh; this test has to be taught the new shape", line != null)

        val words = line!!.split(' ').map { it.trim().trim('"') }.filter { it.isNotEmpty() }
        val flags = words.filter { it.startsWith("-") && it != "-o" }

        assertEquals(
            (TinyCBuild.CC_ARGUMENTS + TinyCBuild.CC_LIBRARIES).toSortedSet(),
            flags.toSortedSet(),
        )
    }

    /** One object format per platform, and the scripts name both. */
    @Test
    fun `the object formats are the ones the scripts ask nasm for`() {
        read("scripts/build.ps1")?.let { assertTrue(it.contains("-f win64")) }
        read("scripts/build.sh")?.let { assertTrue(it.contains("-f elf64")) }
    }

    private fun read(relative: String): String? {
        val repository = System.getProperty("tinyc.repo")?.let { Path.of(it) }
        assumeTrue("the repository is not next to the plugin", repository != null)
        val file = repository!!.resolve(relative)
        assumeTrue("$relative is not there", Files.isRegularFile(file))
        return Files.readString(file)
    }
}
