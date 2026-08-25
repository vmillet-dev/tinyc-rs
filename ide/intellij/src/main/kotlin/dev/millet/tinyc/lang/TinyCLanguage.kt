package dev.millet.tinyc.lang

import com.intellij.lang.Language

/**
 * The language itself. Everything the plugin registers hangs off this one
 * instance: the file type, the lexer, the parser, the completion.
 */
object TinyCLanguage : Language("TinyC") {
    private fun readResolve(): Any = TinyCLanguage
    override fun getDisplayName(): String = "TinyC"
    override fun isCaseSensitive(): Boolean = true
}
