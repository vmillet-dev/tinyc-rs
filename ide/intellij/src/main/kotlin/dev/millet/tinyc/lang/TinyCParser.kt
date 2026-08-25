package dev.millet.tinyc.lang

import com.intellij.lang.ASTNode
import com.intellij.lang.PsiBuilder
import com.intellij.lang.PsiParser
import com.intellij.psi.tree.IElementType
import com.intellij.psi.tree.TokenSet

/**
 * A tolerant parser: it never reports a mistake, it only stops making structure
 * where there is none to make.
 *
 * That is a deliberate division of labour. `tinyc` is the only thing that
 * decides whether a program is wrong — it has the whole language in it and says
 * *why* — so a second opinion from a parser written to keep an editor busy
 * would only ever be a worse copy of it, and would show a red squiggle on
 * perfectly good code the day the language grows. What this one is for is
 * knowing where the declarations are, which is what completion, the structure
 * view and brace matching need, and which is still true of a file being typed
 * into halfway through a word.
 */
class TinyCParser : PsiParser {
    override fun parse(root: IElementType, builder: PsiBuilder): ASTNode {
        val file = builder.mark()
        val parsing = Parsing(builder)
        while (!builder.eof()) {
            val before = builder.currentOffset
            parsing.declaration()
            if (builder.currentOffset == before) builder.advanceLexer()
        }
        file.done(root)
        return builder.treeBuilt
    }
}

private val STATEMENT_START = TokenSet.create(
    TinyCTokens.IF_KW, TinyCTokens.WHILE_KW, TinyCTokens.FOR_KW, TinyCTokens.RETURN_KW,
    TinyCTokens.BREAK_KW, TinyCTokens.CONTINUE_KW, TinyCTokens.MATCH_KW,
    TinyCTokens.PRINT_KW, TinyCTokens.PRINTLN_KW, TinyCTokens.PUSH_KW,
)

private val CLOSERS = TokenSet.create(TinyCTokens.RPAREN, TinyCTokens.RBRACE, TinyCTokens.RBRACKET)

private class Parsing(private val b: PsiBuilder) {
    private var lastConsumed: IElementType? = null

    // -- the shape of a file ------------------------------------------------

    fun declaration() {
        when (b.tokenType) {
            TinyCTokens.FN_KW -> function()
            TinyCTokens.CLASS_KW -> classDecl()
            TinyCTokens.ENUM_KW -> enumDecl()
            else -> advance()
        }
    }

    private fun function() {
        val m = b.mark()
        advance() // fn
        expect(TinyCTokens.IDENTIFIER)
        if (at(TinyCTokens.LPAREN)) params()
        if (at(TinyCTokens.ARROW)) {
            advance()
            // The arrow stays outside, so the node holds the type and nothing
            // else — it is read as text wherever a return type is shown.
            val ret = b.mark()
            type()
            ret.done(TinyCElements.RET_TYPE)
        }
        if (at(TinyCTokens.LBRACE)) block()
        m.done(TinyCElements.FN_DECL)
    }

    private fun params() {
        val m = b.mark()
        advance() // (
        while (!b.eof() && !at(TinyCTokens.RPAREN)) {
            val before = b.currentOffset
            if (at(TinyCTokens.COMMA)) {
                advance()
                continue
            }
            param()
            if (b.currentOffset == before) advance()
        }
        expect(TinyCTokens.RPAREN)
        m.done(TinyCElements.PARAM_LIST)
    }

    /** `int n`, `Shape s`, `int[] xs` — or `self`, which has no type at all. */
    private fun param() {
        val m = b.mark()
        val bare = at(TinyCTokens.IDENTIFIER) &&
            (b.lookAhead(1) == TinyCTokens.RPAREN || b.lookAhead(1) == TinyCTokens.COMMA)
        if (bare) {
            advance()
        } else {
            type()
            expect(TinyCTokens.IDENTIFIER)
        }
        m.done(TinyCElements.PARAM)
    }

    private fun classDecl() {
        val m = b.mark()
        advance() // class
        expect(TinyCTokens.IDENTIFIER)
        if (at(TinyCTokens.COLON)) {
            advance()
            val base = b.mark()
            expect(TinyCTokens.IDENTIFIER)
            base.done(TinyCElements.BASE_CLASS)
        }
        if (at(TinyCTokens.LBRACE)) {
            advance()
            while (!b.eof() && !at(TinyCTokens.RBRACE)) {
                val before = b.currentOffset
                if (at(TinyCTokens.FN_KW)) function() else field()
                if (b.currentOffset == before) advance()
            }
            expect(TinyCTokens.RBRACE)
        }
        m.done(TinyCElements.CLASS_DECL)
    }

    private fun field() {
        if (!looksLikeDeclaration()) {
            advance()
            return
        }
        val m = b.mark()
        type()
        expect(TinyCTokens.IDENTIFIER)
        expect(TinyCTokens.SEMI)
        m.done(TinyCElements.FIELD_DECL)
    }

    private fun enumDecl() {
        val m = b.mark()
        advance() // enum
        expect(TinyCTokens.IDENTIFIER)
        if (at(TinyCTokens.LBRACE)) {
            advance()
            while (!b.eof() && !at(TinyCTokens.RBRACE)) {
                val before = b.currentOffset
                if (at(TinyCTokens.COMMA)) {
                    advance()
                    continue
                }
                variant()
                if (b.currentOffset == before) advance()
            }
            expect(TinyCTokens.RBRACE)
        }
        m.done(TinyCElements.ENUM_DECL)
    }

    private fun variant() {
        if (!at(TinyCTokens.IDENTIFIER)) {
            advance()
            return
        }
        val m = b.mark()
        advance() // name
        if (at(TinyCTokens.LPAREN)) {
            advance()
            while (!b.eof() && !at(TinyCTokens.RPAREN)) {
                val before = b.currentOffset
                if (at(TinyCTokens.COMMA)) advance() else type()
                if (b.currentOffset == before) advance()
            }
            expect(TinyCTokens.RPAREN)
        }
        m.done(TinyCElements.ENUM_VARIANT)
    }

    // -- statements ---------------------------------------------------------

    private fun block() {
        val m = b.mark()
        expect(TinyCTokens.LBRACE)
        while (!b.eof() && !at(TinyCTokens.RBRACE)) {
            val before = b.currentOffset
            statement()
            if (b.currentOffset == before) advance()
        }
        expect(TinyCTokens.RBRACE)
        m.done(TinyCElements.BLOCK)
    }

    private fun statement() {
        when {
            at(TinyCTokens.LBRACE) -> block()
            at(TinyCTokens.IF_KW) -> ifStatement()
            at(TinyCTokens.WHILE_KW) -> loop(TinyCElements.WHILE_STMT)
            at(TinyCTokens.FOR_KW) -> forStatement()
            at(TinyCTokens.RETURN_KW) -> simple(TinyCElements.RETURN_STMT)
            at(TinyCTokens.BREAK_KW) || at(TinyCTokens.CONTINUE_KW) -> simple(TinyCElements.JUMP_STMT)
            looksLikeDeclaration() -> varDecl()
            else -> simple(TinyCElements.EXPR_STMT)
        }
    }

    private fun simple(kind: IElementType) {
        val m = b.mark()
        expression(TokenSet.create(TinyCTokens.SEMI))
        expect(TinyCTokens.SEMI)
        m.done(kind)
    }

    private fun varDecl() {
        val m = b.mark()
        type()
        expect(TinyCTokens.IDENTIFIER)
        if (at(TinyCTokens.EQ)) {
            advance()
            expression(TokenSet.create(TinyCTokens.SEMI))
        }
        expect(TinyCTokens.SEMI)
        m.done(TinyCElements.VAR_DECL)
    }

    private fun ifStatement() {
        val m = b.mark()
        advance() // if
        if (at(TinyCTokens.LPAREN)) balanced()
        if (at(TinyCTokens.LBRACE)) block()
        while (at(TinyCTokens.ELSE_KW)) {
            advance()
            when {
                at(TinyCTokens.LBRACE) -> block()
                at(TinyCTokens.IF_KW) -> ifStatement()
                else -> break
            }
        }
        m.done(TinyCElements.IF_STMT)
    }

    private fun loop(kind: IElementType) {
        val m = b.mark()
        advance()
        if (at(TinyCTokens.LPAREN)) balanced()
        if (at(TinyCTokens.LBRACE)) block()
        m.done(kind)
    }

    /**
     * `for (int i = 0; i < n; i = i + 1) { ... }`.
     *
     * The header is parsed rather than skipped so that `i` is a declaration the
     * editor knows about — it is in scope in the body, which is where someone
     * asks for it.
     */
    private fun forStatement() {
        val m = b.mark()
        advance() // for
        if (at(TinyCTokens.LPAREN)) {
            advance()
            if (looksLikeDeclaration()) {
                varDecl()
            } else {
                expression(TokenSet.create(TinyCTokens.SEMI))
                expect(TinyCTokens.SEMI)
            }
            expression(TokenSet.create(TinyCTokens.SEMI))
            expect(TinyCTokens.SEMI)
            expression(TokenSet.EMPTY)
            expect(TinyCTokens.RPAREN)
        }
        if (at(TinyCTokens.LBRACE)) block()
        m.done(TinyCElements.FOR_STMT)
    }

    // -- expressions, as far as structure goes ------------------------------

    /**
     * Consume an expression, stopping before any of [stops] or before a closing
     * bracket this expression did not open.
     *
     * Only the two shapes that carry declarations of their own are given
     * structure: a `match`, whose arms bind the names a pattern takes apart,
     * and an object literal, which is where a `{` may follow an expression.
     */
    private fun expression(stops: TokenSet) {
        while (!b.eof()) {
            val t = b.tokenType ?: return
            when {
                stops.contains(t) -> return
                t === TinyCTokens.MATCH_KW -> matchExpression()
                t === TinyCTokens.LPAREN || t === TinyCTokens.LBRACKET -> balanced()
                t === TinyCTokens.LBRACE ->
                    // `Circle { r: 5 }` — a brace that follows a name is an
                    // object literal; any other belongs to whoever called us.
                    if (lastConsumed === TinyCTokens.IDENTIFIER) balanced() else return
                CLOSERS.contains(t) -> return
                else -> advance()
            }
        }
    }

    /** Consume from the bracket under the cursor to the one that closes it. */
    private fun balanced() {
        var depth = 0
        while (!b.eof()) {
            val t = b.tokenType
            when {
                t === TinyCTokens.LPAREN || t === TinyCTokens.LBRACKET || t === TinyCTokens.LBRACE -> depth++
                CLOSERS.contains(t) -> depth--
            }
            advance()
            if (depth == 0) return
        }
    }

    private fun matchExpression() {
        val m = b.mark()
        advance() // match
        if (at(TinyCTokens.LPAREN)) balanced()
        if (at(TinyCTokens.LBRACE)) {
            advance()
            while (!b.eof() && !at(TinyCTokens.RBRACE)) {
                val before = b.currentOffset
                arm()
                if (b.currentOffset == before) advance()
            }
            expect(TinyCTokens.RBRACE)
        }
        m.done(TinyCElements.MATCH)
    }

    private fun arm() {
        val m = b.mark()
        pattern()
        expect(TinyCTokens.FAT_ARROW)
        if (at(TinyCTokens.LBRACE)) {
            block()
        } else {
            expression(TokenSet.create(TinyCTokens.COMMA))
        }
        if (at(TinyCTokens.COMMA)) advance()
        m.done(TinyCElements.MATCH_ARM)
    }

    /** `Shape::Rect(width, height)`, `"get"`, `_` — up to the fat arrow. */
    private fun pattern() {
        val m = b.mark()
        while (!b.eof() && !at(TinyCTokens.FAT_ARROW) && !at(TinyCTokens.RBRACE)) {
            if (at(TinyCTokens.LPAREN)) bindings() else advance()
        }
        m.done(TinyCElements.PATTERN)
    }

    private fun bindings() {
        advance() // (
        while (!b.eof() && !at(TinyCTokens.RPAREN)) {
            val before = b.currentOffset
            if (at(TinyCTokens.IDENTIFIER)) {
                val m = b.mark()
                advance()
                m.done(TinyCElements.PATTERN_BINDING)
            } else {
                advance()
            }
            if (b.currentOffset == before) advance()
        }
        expect(TinyCTokens.RPAREN)
    }

    // -- types --------------------------------------------------------------

    /** `int`, `string`, `Circle`, `int[3]`, `Shape[]`. */
    private fun type() {
        if (!atTypeName()) return
        val m = b.mark()
        advance()
        while (at(TinyCTokens.LBRACKET)) {
            advance()
            if (at(TinyCTokens.INT_LITERAL)) advance()
            if (!at(TinyCTokens.RBRACKET)) break
            advance()
        }
        m.done(TinyCElements.TYPE_REF)
    }

    /**
     * Whether what is under the cursor is a type followed by a name — which is
     * the one thing that tells `int x = 1;` from `int(s)` and `x = 1;`.
     */
    private fun looksLikeDeclaration(): Boolean {
        if (!atTypeName()) return false
        var ahead = 1
        while (b.lookAhead(ahead) === TinyCTokens.LBRACKET) {
            ahead++
            if (b.lookAhead(ahead) === TinyCTokens.INT_LITERAL) ahead++
            if (b.lookAhead(ahead) !== TinyCTokens.RBRACKET) return false
            ahead++
        }
        return b.lookAhead(ahead) === TinyCTokens.IDENTIFIER
    }

    private fun atTypeName(): Boolean {
        val t = b.tokenType ?: return false
        return t === TinyCTokens.IDENTIFIER || TinyCTokens.TYPE_TOKENS.contains(t)
    }

    // -- the builder, wrapped ----------------------------------------------

    private fun at(type: IElementType): Boolean = b.tokenType === type

    private fun advance() {
        lastConsumed = b.tokenType
        b.advanceLexer()
    }

    private fun expect(type: IElementType): Boolean {
        if (!at(type)) return false
        advance()
        return true
    }
}
