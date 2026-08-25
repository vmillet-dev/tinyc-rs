package dev.millet.tinyc

import com.intellij.testFramework.fixtures.BasePlatformTestCase
import dev.millet.tinyc.psi.TinyCFile

/**
 * What the editor offers, and where it gets it: the file itself.
 *
 * Each of these is also a test of the parser underneath — a suggestion can only
 * appear if the declaration it came from was recognised.
 */
class TinyCCompletionTest : BasePlatformTestCase() {

    fun testTheFieldsAndMethodsOfAValue() {
        complete(
            """
            class Shape {
              int sides;
              fn area(self) -> int { return 0; }
            }

            fn main() {
              Shape s = Shape { sides: 3 };
              println(s.<caret>);
            }
            """,
        )
        assertContainsElements(strings(), "sides", "area")
    }

    /** A subclass offers what it inherited, and its own. */
    fun testWhatABaseClassDeclared() {
        complete(
            """
            class Shape { int sides; fn area(self) -> int { return 0; } }
            class Rect : Shape { int w; }

            fn main() {
              Rect r = Rect { sides: 4, w: 2 };
              println(r.<caret>);
            }
            """,
        )
        assertContainsElements(strings(), "sides", "w", "area")
    }

    fun testSelfInsideAMethod() {
        complete(
            """
            class Rect {
              int w;
              fn area(self) -> int { return self.<caret>; }
            }
            """,
        )
        assertContainsElements(strings(), "w", "area")
    }

    fun testTheVariantsOfAnEnum() {
        complete(
            """
            enum Colour { Red, Green, Blue }

            fn main() {
              Colour c = Colour::<caret>;
            }
            """,
        )
        assertSameElements(strings(), "Red", "Green", "Blue")
    }

    /** Every field has to be named in an object literal, so it is a checklist. */
    fun testTheFieldsAnObjectLiteralStillNeeds() {
        complete(
            """
            class Rect { int w; int h; }

            fn main() {
              Rect r = Rect { <caret> };
            }
            """,
        )
        assertSameElements(strings(), "w", "h")
    }

    fun testLocalsParametersAndFunctionsInScope() {
        complete(
            """
            fn twice(int n) -> int { return n + n; }

            fn main() {
              int total = 0;
              println(<caret>);
            }
            """,
        )
        assertContainsElements(strings(), "total", "twice", "read_line", "is_int", "match", "true")
    }

    /** A pattern's bindings live only in the arm that named them. */
    fun testWhatAPatternBound() {
        complete(
            """
            enum Shape { Circle(int), Empty }

            fn area(Shape s) -> int {
              return match (s) {
                Shape::Circle(radius) => <caret>,
                Shape::Empty => 0,
              };
            }
            """,
        )
        assertContainsElements(strings(), "radius", "s")
    }

    /** Functions live at the top level, so nothing else is offered there. */
    fun testOnlyDeclarationsAtTheTopLevel() {
        complete(
            """
            class Rect { int w; }
            <caret>
            """,
        )
        val offered = strings()
        assertContainsElements(offered, "fn", "class", "enum")
        assertDoesntContain(offered, "return", "while")
    }

    fun testTheTypesAfterAnArrow() {
        complete(
            """
            class Rect { int w; }
            enum Colour { Red }

            fn make() -> <caret>
            """,
        )
        assertContainsElements(strings(), "int", "string", "char", "bool", "Rect", "Colour")
    }

    /** The tree is what completion reads, so it is worth asserting directly. */
    fun testTheParserFindsEveryDeclaration() {
        myFixture.configureByText(
            "a.tc",
            """
            enum Colour { Red, Green }
            class Shape { int sides; fn area(self) -> int { return 0; } }
            class Rect : Shape { int w; }
            fn main() { }
            """.trimIndent(),
        )
        val file = myFixture.file as TinyCFile
        assertEquals(listOf("main"), file.functions.map { it.name })
        assertEquals(listOf("Shape", "Rect"), file.classes.map { it.name })
        assertEquals(listOf("Colour"), file.enums.map { it.name })
        assertEquals("Shape", file.classes[1].baseName)
        assertEquals(listOf("Red", "Green"), file.enums[0].variants.map { it.name })
        assertEquals(listOf("sides"), file.classes[0].fields.map { it.name })
        assertEquals("area(self) -> int", file.classes[0].methods[0].signature())
    }

    private fun complete(source: String) {
        myFixture.configureByText("a.tc", source.trimIndent())
        myFixture.completeBasic()
    }

    private fun strings(): List<String> = myFixture.lookupElementStrings ?: emptyList()
}
