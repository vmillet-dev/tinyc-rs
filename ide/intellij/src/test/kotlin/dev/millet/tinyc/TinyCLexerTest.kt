package dev.millet.tinyc

import com.intellij.psi.TokenType
import com.intellij.psi.tree.IElementType
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import dev.millet.tinyc.lang.TinyCLexer
import dev.millet.tinyc.lang.TinyCTokens

class TinyCLexerTest : BasePlatformTestCase() {

    fun testADeclaration() {
        assertEquals(
            listOf(TinyCTokens.INT_KW, TinyCTokens.IDENTIFIER, TinyCTokens.EQ, TinyCTokens.INT_LITERAL, TinyCTokens.SEMI),
            tokens("int x = 10;"),
        )
    }

    fun testAKeywordIsNeverRecognisedAsAPrefix() {
        // `integer` starts with `int` and is a name; the match runs on the whole
        // word, exactly as the compiler's does.
        assertEquals(listOf(TinyCTokens.IDENTIFIER, TinyCTokens.IDENTIFIER), tokens("integer printer"))
    }

    fun testTheLongestOperatorWins() {
        assertEquals(
            listOf(
                TinyCTokens.ARROW, TinyCTokens.FAT_ARROW, TinyCTokens.COLON_COLON, TinyCTokens.COLON,
                TinyCTokens.EQ_EQ, TinyCTokens.BANG_EQ, TinyCTokens.LE, TinyCTokens.GE,
                TinyCTokens.AND_AND, TinyCTokens.OR_OR, TinyCTokens.LT, TinyCTokens.GT, TinyCTokens.EQ,
            ),
            tokens("-> => :: : == != <= >= && || < > ="),
        )
    }

    fun testACommentRunsToTheEndOfTheLine() {
        assertEquals(
            listOf(TinyCTokens.LINE_COMMENT, TinyCTokens.INT_KW),
            tokens("// int x = 1;\nint"),
        )
    }

    fun testAStringKeepsItsEscapes() {
        assertEquals(listOf(TinyCTokens.STRING_LITERAL), tokens(""""a \" b \\ c""""))
    }

    /**
     * The compiler refuses an unterminated literal; this lexer still hands the
     * editor a token, because colouring has to carry on past a line that is
     * being typed.
     */
    fun testAnUnterminatedStringStopsAtTheEndOfTheLine() {
        assertEquals(
            listOf(TinyCTokens.STRING_LITERAL, TinyCTokens.INT_KW),
            tokens("\"no closing quote\nint"),
        )
    }

    /** `123abc` is one malformed literal, not a literal and a name. */
    fun testASuffixBelongsToTheLiteral() {
        assertEquals(listOf(TinyCTokens.INT_LITERAL), tokens("123abc"))
    }

    fun testAnUnknownCharacterIsOneBadToken() {
        assertEquals(
            listOf(TinyCTokens.IDENTIFIER, TokenType.BAD_CHARACTER, TinyCTokens.IDENTIFIER),
            tokens("a # b"),
        )
    }

    fun testAccentedNamesAreNames() {
        assertEquals(listOf(TinyCTokens.IDENTIFIER), tokens("café"))
    }

    /** Every character of the file belongs to exactly one token. */
    fun testTheTokensCoverTheWholeText() {
        val text = "fn main() {\n  println(\"hé %d\", 1); // done\n}\n"
        val lexer = TinyCLexer()
        lexer.start(text)
        var at = 0
        while (lexer.tokenType != null) {
            assertEquals(at, lexer.tokenStart)
            assertTrue(lexer.tokenEnd > lexer.tokenStart)
            at = lexer.tokenEnd
            lexer.advance()
        }
        assertEquals(text.length, at)
    }

    private fun tokens(text: String): List<IElementType> {
        val lexer = TinyCLexer()
        lexer.start(text)
        val found = ArrayList<IElementType>()
        while (true) {
            val type = lexer.tokenType ?: break
            if (type != TokenType.WHITE_SPACE) found += type
            lexer.advance()
        }
        return found
    }
}
