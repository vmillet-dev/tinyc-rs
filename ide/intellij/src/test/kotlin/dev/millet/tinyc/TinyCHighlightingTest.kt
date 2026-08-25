package dev.millet.tinyc

import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import dev.millet.tinyc.lang.TinyCColors

/**
 * What the editor paints, and — just as much the point — what it refuses to
 * complain about.
 */
class TinyCHighlightingTest : BasePlatformTestCase() {

    /**
     * Nothing in the plugin decides that a program is wrong. Only the compiler
     * does, through the external annotator, and it has no repository to run in
     * here — so a whole program must come back with no error on it at all.
     */
    fun testTheEditorItselfNeverCallsAProgramWrong() {
        myFixture.configureByText("a.tc", PROGRAM)
        val errors = myFixture.doHighlighting().filter { it.severity == HighlightSeverity.ERROR }
        assertEmpty(errors)
    }

    /**
     * And that has to hold for a file being typed into, which is most of them:
     * an unclosed brace, a half-written call and a missing semicolon are what
     * the editor sees between one keystroke and the next.
     */
    fun testAHalfWrittenFileIsNotAnError() {
        myFixture.configureByText(
            "a.tc",
            """
            class Rect {
              int w
              fn area(self) -> int { return self.

            fn main() {
              int x =
            """.trimIndent(),
        )
        val errors = myFixture.doHighlighting().filter { it.severity == HighlightSeverity.ERROR }
        assertEmpty(errors)
    }

    /** A `%d` in a format is coloured as the specifier it is. */
    fun testTheSpecifiersInAFormatArePainted() {
        val source = """
            fn main() {
              println("n = %d", 1);
            }
        """.trimIndent()
        myFixture.configureByText("a.tc", source)

        val at = source.indexOf("%d")
        val painted = myFixture.doHighlighting().any {
            it.startOffset == at && it.endOffset == at + 2 &&
                it.forcedTextAttributesKey == TinyCColors.FORMAT_SPECIFIER
        }
        assertTrue("the %d in the format should be painted as a specifier", painted)
    }

    /** A `%` outside a format is text, not a specifier. */
    fun testAPercentInAPlainStringIsNotASpecifier() {
        val source = """
            fn main() {
              string s = "100%d sure";
              println(s);
            }
        """.trimIndent()
        myFixture.configureByText("a.tc", source)

        val at = source.indexOf("%d")
        val painted = myFixture.doHighlighting().any {
            it.startOffset == at && it.forcedTextAttributesKey == TinyCColors.FORMAT_SPECIFIER
        }
        assertFalse("only a literal in first position is a format", painted)
    }

    private companion object {
        val PROGRAM = """
            // Every shape this language has, in one file.
            enum Parsed {
              Ok(int),
              Bad(string),
            }

            class Shape {
              int sides;
              fn area(self) -> int { return 0; }
            }

            class Rect : Shape {
              int w;
              int h;
              fn area(self) -> int { return self.w * self.h; }
            }

            fn parse(string text) -> Parsed {
              if (is_int(text)) {
                return Parsed::Ok(int(text));
              }
              return Parsed::Bad(text);
            }

            fn describe(Parsed p) -> string {
              return match (p) {
                Parsed::Ok(n) => string(n),
                Parsed::Bad(why) => why,
              };
            }

            fn main() {
              Rect r = Rect { sides: 4, w: 3, h: 7 };
              Shape s = r;
              println("area %d", s.area());

              int[] xs = [];
              for (int i = 0; i < 4; i = i + 1) {
                push(xs, i * i);
              }
              while (len(xs) > 0 && !eof()) {
                println("%s", describe(parse(read_line())));
                break;
              }

              char c = 'x';
              bool ok = true;
              println("%c %b", c, ok);
            }
        """.trimIndent()
    }
}
