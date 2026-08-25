package dev.millet.tinyc.lang

import com.intellij.lexer.LexerBase
import com.intellij.psi.TokenType
import com.intellij.psi.tree.IElementType

/**
 * A hand-written lexer, mirroring the *shape* of `src/lexer.rs`.
 *
 * What it deliberately does not mirror is the vocabulary: the words and the
 * symbols are read out of `TinyCTokens`, which is generated from the compiler's
 * own token table. So the rules for what a comment, a literal and a name look
 * like live here — they have not changed since the language began — while what
 * counts as a keyword or an operator lives in `src/token.rs` and arrives by
 * itself.
 *
 * It is stateless between tokens — no token in TinyC spans a line, since a
 * comment ends at one and a string literal may not cross one — so the editor
 * may restart it at any token boundary, which is what makes typing in a large
 * file cheap.
 *
 * Where the compiler's lexer *refuses* (an unterminated string, `123abc`, a
 * character that begins nothing), this one still produces the token it was
 * building: colouring would stop at the first refusal otherwise, and the
 * message about it is the compiler's to give — see `TinyCExternalAnnotator`.
 */
class TinyCLexer : LexerBase() {
    private var buffer: CharSequence = ""
    private var bufferEnd = 0
    private var tokenStart = 0
    private var tokenEnd = 0
    private var tokenType: IElementType? = null

    override fun start(buffer: CharSequence, startOffset: Int, endOffset: Int, initialState: Int) {
        this.buffer = buffer
        this.bufferEnd = endOffset
        this.tokenEnd = startOffset
        advance()
    }

    override fun getState(): Int = 0
    override fun getTokenType(): IElementType? = tokenType
    override fun getTokenStart(): Int = tokenStart
    override fun getTokenEnd(): Int = tokenEnd
    override fun getBufferSequence(): CharSequence = buffer
    override fun getBufferEnd(): Int = bufferEnd

    override fun advance() {
        tokenStart = tokenEnd
        tokenType = if (tokenStart >= bufferEnd) null else scan()
    }

    private fun at(offset: Int): Char? = if (offset < bufferEnd) buffer[offset] else null

    private fun scan(): IElementType {
        var pos = tokenStart
        val c = buffer[pos]

        if (c.isWhitespace()) {
            while (at(pos)?.isWhitespace() == true) pos++
            return finish(pos, TokenType.WHITE_SPACE)
        }

        if (c == SLASH_CHAR && at(pos + 1) == SLASH_CHAR) {
            while (pos < bufferEnd && buffer[pos] != NEWLINE) pos++
            return finish(pos, TinyCTokens.LINE_COMMENT)
        }

        if (c == DOUBLE_QUOTE) return quoted(pos, DOUBLE_QUOTE, TinyCTokens.STRING_LITERAL)
        if (c == SINGLE_QUOTE) return quoted(pos, SINGLE_QUOTE, TinyCTokens.CHAR_LITERAL)

        if (c.isDigit()) {
            while (at(pos)?.isDigit() == true) pos++
            // `123abc` is a malformed literal, not a literal followed by a name
            // — the compiler says so, so it has to be one token here too.
            while (at(pos)?.let { isIdentContinue(it) } == true) pos++
            return finish(pos, TinyCTokens.INT_LITERAL)
        }

        if (isIdentStart(c)) {
            while (at(pos)?.let { isIdentContinue(it) } == true) pos++
            val word = buffer.subSequence(tokenStart, pos).toString()
            return finish(pos, TinyCTokens.KEYWORDS[word] ?: TinyCTokens.IDENTIFIER)
        }

        // Punctuation, longest match first — which is the order the table is
        // already in, so `->` is tried before `-` without this knowing why.
        for ((spelling, type) in TinyCTokens.PUNCTUATION) {
            if (matches(pos, spelling)) return finish(pos + spelling.length, type)
        }
        return finish(pos + 1, TokenType.BAD_CHARACTER)
    }

    private fun matches(pos: Int, spelling: String): Boolean {
        if (pos + spelling.length > bufferEnd) return false
        for (i in spelling.indices) {
            if (buffer[pos + i] != spelling[i]) return false
        }
        return true
    }

    /**
     * A literal between [quote]s. An escape covers the character after the
     * backslash, and the literal stops at the end of the line when the closing
     * quote never comes — exactly where the compiler stops looking for it.
     */
    private fun quoted(start: Int, quote: Char, type: IElementType): IElementType {
        var pos = start + 1
        while (pos < bufferEnd) {
            val c = buffer[pos]
            when {
                c == NEWLINE -> return finish(pos, type)
                c == BACKSLASH -> pos += if (pos + 1 < bufferEnd && buffer[pos + 1] != NEWLINE) 2 else 1
                c == quote -> return finish(pos + 1, type)
                else -> pos++
            }
        }
        return finish(pos, type)
    }

    private fun finish(end: Int, type: IElementType): IElementType {
        tokenEnd = end.coerceAtMost(bufferEnd)
        return type
    }

    companion object {
        private const val NEWLINE = '\n'
        private const val BACKSLASH = '\\'
        private const val DOUBLE_QUOTE = '"'
        private const val SINGLE_QUOTE = '\''
        private const val SLASH_CHAR = '/'

        fun isIdentStart(c: Char): Boolean = c == '_' || c.isLetter()
        fun isIdentContinue(c: Char): Boolean = c == '_' || c.isLetterOrDigit()
    }
}
