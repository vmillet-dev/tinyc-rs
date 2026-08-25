package dev.millet.tinyc.lang

import com.intellij.lexer.Lexer
import com.intellij.openapi.editor.DefaultLanguageHighlighterColors as Default
import com.intellij.openapi.editor.HighlighterColors
import com.intellij.openapi.editor.colors.TextAttributesKey
import com.intellij.openapi.editor.colors.TextAttributesKey.createTextAttributesKey
import com.intellij.openapi.fileTypes.SyntaxHighlighter
import com.intellij.openapi.fileTypes.SyntaxHighlighterBase
import com.intellij.openapi.fileTypes.SyntaxHighlighterFactory
import com.intellij.openapi.project.Project
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.psi.TokenType
import com.intellij.psi.tree.IElementType

/**
 * The colours, named after what they mean rather than after a colour, so the
 * theme decides how each one looks.
 *
 * Every key inherits from one of the platform's own, which is what makes a
 * TinyC keyword look like a keyword in whatever scheme is in use — until
 * someone overrides it in Settings | Editor | Color Scheme | TinyC.
 */
object TinyCColors {
    val KEYWORD = key("TINYC_KEYWORD", Default.KEYWORD)
    val TYPE = key("TINYC_TYPE", Default.KEYWORD)
    val CONSTRUCT = key("TINYC_CONSTRUCT", Default.KEYWORD)
    val NUMBER = key("TINYC_NUMBER", Default.NUMBER)
    val STRING = key("TINYC_STRING", Default.STRING)
    val CHAR = key("TINYC_CHAR", Default.STRING)
    val FORMAT_SPECIFIER = key("TINYC_FORMAT_SPECIFIER", Default.VALID_STRING_ESCAPE)
    val COMMENT = key("TINYC_COMMENT", Default.LINE_COMMENT)
    val IDENTIFIER = key("TINYC_IDENTIFIER", Default.IDENTIFIER)
    val FUNCTION_DECLARATION = key("TINYC_FUNCTION_DECLARATION", Default.FUNCTION_DECLARATION)
    val FUNCTION_CALL = key("TINYC_FUNCTION_CALL", Default.FUNCTION_CALL)
    val METHOD_CALL = key("TINYC_METHOD_CALL", Default.INSTANCE_METHOD)
    val BUILTIN = key("TINYC_BUILTIN", Default.PREDEFINED_SYMBOL)
    val CLASS_NAME = key("TINYC_CLASS_NAME", Default.CLASS_NAME)
    val ENUM_VARIANT = key("TINYC_ENUM_VARIANT", Default.STATIC_FIELD)
    val FIELD = key("TINYC_FIELD", Default.INSTANCE_FIELD)
    val PARAMETER = key("TINYC_PARAMETER", Default.PARAMETER)
    val OPERATOR = key("TINYC_OPERATOR", Default.OPERATION_SIGN)
    val PARENTHESES = key("TINYC_PARENTHESES", Default.PARENTHESES)
    val BRACES = key("TINYC_BRACES", Default.BRACES)
    val BRACKETS = key("TINYC_BRACKETS", Default.BRACKETS)
    val SEMICOLON = key("TINYC_SEMICOLON", Default.SEMICOLON)
    val COMMA = key("TINYC_COMMA", Default.COMMA)
    val DOT = key("TINYC_DOT", Default.DOT)
    val BAD_CHARACTER = key("TINYC_BAD_CHARACTER", HighlighterColors.BAD_CHARACTER)

    private fun key(name: String, fallback: TextAttributesKey) = createTextAttributesKey(name, fallback)
}

class TinyCSyntaxHighlighter : SyntaxHighlighterBase() {
    override fun getHighlightingLexer(): Lexer = TinyCLexer()

    override fun getTokenHighlights(tokenType: IElementType): Array<TextAttributesKey> = when {
        // One arm per role the compiler declares; the sets are generated from
        // it, and only which colour a role gets is decided here.
        TinyCTokens.TYPE_TOKENS.contains(tokenType) -> only(TinyCColors.TYPE)
        TinyCTokens.CONSTRUCT_TOKENS.contains(tokenType) -> only(TinyCColors.CONSTRUCT)
        TinyCTokens.CONTROL_TOKENS.contains(tokenType) -> only(TinyCColors.KEYWORD)
        TinyCTokens.LITERAL_TOKENS.contains(tokenType) -> only(TinyCColors.KEYWORD)
        tokenType == TinyCTokens.INT_LITERAL -> only(TinyCColors.NUMBER)
        tokenType == TinyCTokens.STRING_LITERAL -> only(TinyCColors.STRING)
        tokenType == TinyCTokens.CHAR_LITERAL -> only(TinyCColors.CHAR)
        tokenType == TinyCTokens.LINE_COMMENT -> only(TinyCColors.COMMENT)
        tokenType == TinyCTokens.IDENTIFIER -> only(TinyCColors.IDENTIFIER)
        TinyCTokens.OPERATOR_TOKENS.contains(tokenType) -> only(TinyCColors.OPERATOR)
        tokenType == TinyCTokens.LPAREN || tokenType == TinyCTokens.RPAREN -> only(TinyCColors.PARENTHESES)
        tokenType == TinyCTokens.LBRACE || tokenType == TinyCTokens.RBRACE -> only(TinyCColors.BRACES)
        tokenType == TinyCTokens.LBRACKET || tokenType == TinyCTokens.RBRACKET -> only(TinyCColors.BRACKETS)
        tokenType == TinyCTokens.SEMI -> only(TinyCColors.SEMICOLON)
        tokenType == TinyCTokens.COMMA -> only(TinyCColors.COMMA)
        tokenType == TinyCTokens.DOT || tokenType == TinyCTokens.COLON_COLON -> only(TinyCColors.DOT)
        tokenType == TokenType.BAD_CHARACTER -> only(TinyCColors.BAD_CHARACTER)
        else -> emptyArray()
    }

    private fun only(key: TextAttributesKey) = arrayOf(key)
}

class TinyCSyntaxHighlighterFactory : SyntaxHighlighterFactory() {
    override fun getSyntaxHighlighter(project: Project?, virtualFile: VirtualFile?): SyntaxHighlighter =
        TinyCSyntaxHighlighter()
}
