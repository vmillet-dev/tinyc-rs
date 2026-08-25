package dev.millet.tinyc.lang

import com.intellij.psi.tree.IElementType

/**
 * One kind of token, as the platform counts them.
 *
 * Every instance is created by the generated `TinyCTokens`, which is written
 * from `grammar/vocabulary.txt` — the compiler's own token table, exported.
 * Nothing here decides what TinyC's words are; see `src/token.rs`.
 */
class TinyCTokenType(debugName: String) : IElementType(debugName, TinyCLanguage) {
    override fun toString(): String = "TinyC:" + super.toString()
}
