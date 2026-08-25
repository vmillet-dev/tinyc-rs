package dev.millet.tinyc.annotator

import com.intellij.lang.annotation.AnnotationHolder
import com.intellij.lang.annotation.Annotator
import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.openapi.editor.colors.TextAttributesKey
import com.intellij.openapi.util.TextRange
import com.intellij.psi.PsiElement
import com.intellij.psi.util.PsiTreeUtil
import dev.millet.tinyc.lang.TinyCColors
import dev.millet.tinyc.lang.TinyCElements
import dev.millet.tinyc.lang.TinyCTokens
import dev.millet.tinyc.model.TinyCScope
import dev.millet.tinyc.psi.TinyCClassDecl
import dev.millet.tinyc.psi.TinyCEnumDecl
import dev.millet.tinyc.psi.TinyCEnumVariant
import dev.millet.tinyc.psi.TinyCFieldDecl
import dev.millet.tinyc.psi.TinyCFile
import dev.millet.tinyc.psi.TinyCFnDecl
import dev.millet.tinyc.psi.TinyCParam
import dev.millet.tinyc.psi.TinyCPatternBinding
import dev.millet.tinyc.psi.TinyCTypeRef

/**
 * The colouring a lexer cannot do, because it is about what a name *is*.
 *
 * A class, an enum's variant, a field, a parameter and a call all lex as one
 * identifier; telling them apart takes the tree. Nothing here reports a
 * mistake — an unknown name is simply left the colour of a plain identifier,
 * and it is the compiler, through the external annotator, that says whether it
 * exists.
 */
class TinyCHighlightAnnotator : Annotator {
    override fun annotate(element: PsiElement, holder: AnnotationHolder) {
        when (element.node.elementType) {
            TinyCTokens.IDENTIFIER -> identifier(element, holder)
            TinyCTokens.STRING_LITERAL -> formatSpecifiers(element, holder)
        }
    }

    private fun identifier(element: PsiElement, holder: AnnotationHolder) {
        val parent = element.parent
        val declared = when {
            parent is TinyCFnDecl && parent.nameIdentifier === element -> TinyCColors.FUNCTION_DECLARATION
            parent is TinyCClassDecl && parent.nameIdentifier === element -> TinyCColors.CLASS_NAME
            parent is TinyCEnumDecl && parent.nameIdentifier === element -> TinyCColors.CLASS_NAME
            parent is TinyCEnumVariant && parent.nameIdentifier === element -> TinyCColors.ENUM_VARIANT
            parent is TinyCFieldDecl && parent.nameIdentifier === element -> TinyCColors.FIELD
            parent is TinyCParam && parent.nameIdentifier === element -> TinyCColors.PARAMETER
            parent is TinyCPatternBinding -> TinyCColors.PARAMETER
            parent is TinyCTypeRef -> TinyCColors.CLASS_NAME
            else -> null
        }
        if (declared != null) {
            paint(holder, element.textRange, declared)
            return
        }

        val file = element.containingFile as? TinyCFile ?: return
        val before = previousMeaningful(element)?.node?.elementType
        val after = nextMeaningful(element)?.node?.elementType
        val name = element.text

        val used = when {
            // `Colour::Red` — the enum in front, the variant behind.
            after === TinyCTokens.COLON_COLON -> TinyCColors.CLASS_NAME
            before === TinyCTokens.COLON_COLON -> TinyCColors.ENUM_VARIANT

            // A base class, written after the colon of a class header.
            element.parent.node.elementType === TinyCElements.BASE_CLASS -> TinyCColors.CLASS_NAME

            before === TinyCTokens.DOT ->
                if (after === TinyCTokens.LPAREN) TinyCColors.METHOD_CALL else TinyCColors.FIELD

            after === TinyCTokens.LPAREN ->
                if (name in TinyCTokens.BUILTIN_FUNCTIONS) TinyCColors.BUILTIN else TinyCColors.FUNCTION_CALL

            name == "self" -> TinyCColors.BUILTIN
            TinyCScope.classNamed(file, name) != null -> TinyCColors.CLASS_NAME
            TinyCScope.enumNamed(file, name) != null -> TinyCColors.CLASS_NAME

            else -> {
                val declaration = TinyCScope.valuesVisibleAt(element).firstOrNull { it.name == name }
                if (declaration is TinyCParam) TinyCColors.PARAMETER else null
            }
        }
        if (used != null) paint(holder, element.textRange, used)
    }

    /**
     * The `%d` in `println("n = %d", n)`.
     *
     * Only a literal in first position is a format — that is the language's
     * rule, and it is why this can be answered by looking at the two tokens in
     * front rather than at what the argument turns out to be.
     */
    private fun formatSpecifiers(element: PsiElement, holder: AnnotationHolder) {
        val open = previousMeaningful(element) ?: return
        if (open.node.elementType !== TinyCTokens.LPAREN) return
        val construct = previousMeaningful(open)?.node?.elementType
        if (construct !== TinyCTokens.PRINT_KW && construct !== TinyCTokens.PRINTLN_KW) return

        val text = element.text
        val start = element.textRange.startOffset
        var at = 0
        while (at < text.length - 1) {
            if (text[at] == '%' && text[at + 1] in SPECIFIERS) {
                paint(holder, TextRange(start + at, start + at + 2), TinyCColors.FORMAT_SPECIFIER)
                at += 2
            } else {
                at++
            }
        }
    }

    private fun paint(holder: AnnotationHolder, range: TextRange, key: TextAttributesKey) {
        holder.newSilentAnnotation(HighlightSeverity.INFORMATION).range(range).textAttributes(key).create()
    }

    private fun previousMeaningful(element: PsiElement): PsiElement? = step(element, forward = false)

    private fun nextMeaningful(element: PsiElement): PsiElement? = step(element, forward = true)

    private fun step(element: PsiElement, forward: Boolean): PsiElement? {
        var leaf = if (forward) PsiTreeUtil.nextLeaf(element, true) else PsiTreeUtil.prevLeaf(element, true)
        while (leaf != null && (leaf.node.elementType === TinyCTokens.LINE_COMMENT || leaf.text.isBlank())) {
            leaf = if (forward) PsiTreeUtil.nextLeaf(leaf, true) else PsiTreeUtil.prevLeaf(leaf, true)
        }
        return leaf
    }

    private companion object {
        /** One letter per printable type, and `%%` for a percent sign. */
        const val SPECIFIERS = "dcsbe%"
    }
}
