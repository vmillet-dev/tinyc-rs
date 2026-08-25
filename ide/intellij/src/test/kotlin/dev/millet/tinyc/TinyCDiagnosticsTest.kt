package dev.millet.tinyc

import dev.millet.tinyc.annotator.TinyCDiagnostics
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Reading what `tinyc` prints.
 *
 * Every sample here is the compiler's real output, copied from a run — which is
 * the only thing that makes a test like this worth having: the format is not
 * specified anywhere, it is whatever `SourceFile::render` writes.
 */
class TinyCDiagnosticsTest {

    @Test
    fun `a message, a place and a caret`() {
        val output = """
            error: undeclared variable `z`
             --> examples/errors/undeclared_variable.tc:3:13
              |
            3 |   print(x + z);
              |             ^ not declared anywhere above this point
        """.trimIndent()

        val found = TinyCDiagnostics.parse(output)
        assertEquals(1, found.size)
        val diagnostic = found[0]
        assertEquals(3, diagnostic.line)
        assertEquals(13, diagnostic.column)
        assertEquals(1, diagnostic.length)
        assertEquals("undeclared variable `z`", diagnostic.message)
        assertEquals("not declared anywhere above this point", diagnostic.label)
    }

    @Test
    fun `the carets say how wide the mistake is`() {
        val output = """
            error: expected `;`, found `print`
             --> examples/errors/missing_semicolon.tc:3:3
              |
            3 |   print(x);
              |   ^^^^^ expected `;` here
        """.trimIndent()

        val diagnostic = TinyCDiagnostics.parse(output).single()
        assertEquals(5, diagnostic.length)
    }

    /**
     * A note may carry a second snippet, pointing at what the first one is being
     * compared against. The diagnostic still belongs where it started.
     */
    @Test
    fun `a note does not move the diagnostic`() {
        val output = """
            error: cannot write a `string` with `%d`
             --> examples/errors/format_type_mismatch.tc:3:21
              |
            3 |   println("n = %d", name);
              |                     ^^^^ `%d` writes an int
              = note: this is the specifier it has to match
             --> examples/errors/format_type_mismatch.tc:3:16
              |
            3 |   println("n = %d", name);
              |                ^^
        """.trimIndent()

        val diagnostic = TinyCDiagnostics.parse(output).single()
        assertEquals(3, diagnostic.line)
        assertEquals(21, diagnostic.column)
        assertEquals(4, diagnostic.length)
        assertEquals("this is the specifier it has to match", diagnostic.note)
    }

    /** The front end reports every mistake it can find its footing after. */
    @Test
    fun `several mistakes are several diagnostics`() {
        val output = """
            error: undeclared variable `a`
             --> t.tc:2:9
              |
            2 |   print(a);
              |         ^ not declared anywhere above this point

            error: undeclared variable `b`
             --> t.tc:3:9
              |
            3 |   print(b);
              |         ^ not declared anywhere above this point
        """.trimIndent()

        val found = TinyCDiagnostics.parse(output)
        assertEquals(2, found.size)
        assertEquals(2, found[0].line)
        assertEquals(3, found[1].line)
    }

    /**
     * A message about the *run* rather than about the program has no place in
     * the file, and the editor has nowhere to put it.
     */
    @Test
    fun `a failure with no location is not a mark in the editor`() {
        val output = "error: cannot read nowhere.tc: The system cannot find the file specified. (os error 2)\n"
        assertTrue(TinyCDiagnostics.parse(output).isEmpty())
    }
}
