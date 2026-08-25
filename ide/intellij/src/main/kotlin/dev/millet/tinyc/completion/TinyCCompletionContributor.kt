package dev.millet.tinyc.completion

import com.intellij.codeInsight.completion.CompletionContributor
import com.intellij.codeInsight.completion.CompletionParameters
import com.intellij.codeInsight.completion.CompletionProvider
import com.intellij.codeInsight.completion.CompletionResultSet
import com.intellij.codeInsight.completion.CompletionType
import com.intellij.codeInsight.completion.InsertionContext
import com.intellij.codeInsight.lookup.LookupElement
import com.intellij.codeInsight.lookup.LookupElementBuilder
import com.intellij.icons.AllIcons
import com.intellij.patterns.PlatformPatterns
import com.intellij.psi.PsiElement
import com.intellij.psi.util.PsiTreeUtil
import com.intellij.util.ProcessingContext
import dev.millet.tinyc.lang.TinyCLanguage
import dev.millet.tinyc.lang.TinyCTokens
import dev.millet.tinyc.model.TinyCScope
import dev.millet.tinyc.psi.TinyCBlock
import dev.millet.tinyc.psi.TinyCClassDecl
import dev.millet.tinyc.psi.TinyCFile

/**
 * Completion, offered from what the file itself declares.
 *
 * There is no index and no cache behind this: a TinyC program is one file, so
 * everything that could be suggested is in the tree already open in the editor.
 */
class TinyCCompletionContributor : CompletionContributor() {
    init {
        extend(
            CompletionType.BASIC,
            PlatformPatterns.psiElement().withLanguage(TinyCLanguage),
            TinyCCompletionProvider(),
        )
    }
}

private class TinyCCompletionProvider : CompletionProvider<CompletionParameters>() {
    override fun addCompletions(
        parameters: CompletionParameters,
        context: ProcessingContext,
        result: CompletionResultSet,
    ) {
        val position = parameters.position
        val file = position.containingFile as? TinyCFile ?: return

        // Nothing to offer inside a comment or a literal.
        val here = position.node.elementType
        if (here === TinyCTokens.LINE_COMMENT ||
            here === TinyCTokens.STRING_LITERAL ||
            here === TinyCTokens.CHAR_LITERAL
        ) {
            return
        }

        val previous = previousMeaningful(position)
        when {
            previous?.node?.elementType === TinyCTokens.COLON_COLON ->
                enumVariants(file, previous, result)

            previous?.node?.elementType === TinyCTokens.DOT ->
                members(file, position, previous, result)

            previous?.node?.elementType === TinyCTokens.ARROW ->
                types(file, result)

            previous?.node?.elementType === TinyCTokens.COLON &&
                TinyCScope.enclosingClass(position) != null &&
                PsiTreeUtil.getParentOfType(position, TinyCBlock::class.java) == null ->
                classes(file, result)

            else -> {
                val objectLiteral = objectLiteralClass(file, position)
                if (objectLiteral != null) {
                    fields(file, objectLiteral, result)
                } else {
                    everythingInScope(file, position, result)
                }
            }
        }
    }

    // -- the contexts -------------------------------------------------------

    private fun enumVariants(file: TinyCFile, colonColon: PsiElement, result: CompletionResultSet) {
        val name = previousMeaningful(colonColon)?.text ?: return
        val declaration = TinyCScope.enumNamed(file, name) ?: return
        for (variant in declaration.variants) {
            val variantName = variant.name ?: continue
            val payload = variant.payload
            var element = LookupElementBuilder.create(variant, variantName)
                .withIcon(AllIcons.Nodes.Enum)
                .withTypeText(name)
            if (payload.isNotEmpty()) {
                element = element.withTailText("(" + payload.joinToString(", ") + ")", true)
                    .withInsertHandler(::insertCallWithArguments)
            }
            result.addElement(element)
        }
    }

    private fun members(file: TinyCFile, position: PsiElement, dot: PsiElement, result: CompletionResultSet) {
        val receiver = previousMeaningful(dot) ?: return
        if (receiver.node.elementType !== TinyCTokens.IDENTIFIER) return
        val cls = TinyCScope.classOfValue(file, position, receiver.text) ?: return

        for (field in TinyCScope.fieldsOf(file, cls)) {
            val name = field.name ?: continue
            result.addElement(
                LookupElementBuilder.create(field, name)
                    .withIcon(AllIcons.Nodes.Field)
                    .withTypeText(field.typeText ?: ""),
            )
        }
        for (method in TinyCScope.methodsOf(file, cls)) {
            val name = method.name ?: continue
            // `self` is written in the declaration and never at the call.
            val arguments = method.parameters.filter { it.typeText != null }
            result.addElement(
                LookupElementBuilder.create(method, name)
                    .withIcon(AllIcons.Nodes.Method)
                    .withTailText("(" + arguments.joinToString(", ") { it.typeText ?: "" } + ")", true)
                    .withTypeText(method.returnTypeText ?: "")
                    .withInsertHandler(if (arguments.isEmpty()) ::insertEmptyCall else ::insertCallWithArguments),
            )
        }
    }

    private fun fields(file: TinyCFile, cls: TinyCClassDecl, result: CompletionResultSet) {
        // Every field has to be named in an object literal, so this is the one
        // completion in TinyC that is also a checklist.
        for (field in TinyCScope.fieldsOf(file, cls)) {
            val name = field.name ?: continue
            result.addElement(
                LookupElementBuilder.create(field, name)
                    .withIcon(AllIcons.Nodes.Field)
                    .withTypeText(field.typeText ?: "")
                    .withInsertHandler { context, _ ->
                        context.document.insertString(context.tailOffset, ": ")
                        context.editor.caretModel.moveToOffset(context.tailOffset)
                    },
            )
        }
    }

    private fun types(file: TinyCFile, result: CompletionResultSet) {
        for (keyword in TinyCTokens.TYPE_WORDS) {
            result.addElement(LookupElementBuilder.create(keyword).bold())
        }
        classes(file, result)
        for (declaration in file.enums) {
            val name = declaration.name ?: continue
            result.addElement(LookupElementBuilder.create(declaration, name).withIcon(AllIcons.Nodes.Enum))
        }
    }

    private fun classes(file: TinyCFile, result: CompletionResultSet) {
        for (declaration in file.classes) {
            val name = declaration.name ?: continue
            result.addElement(
                LookupElementBuilder.create(declaration, name)
                    .withIcon(AllIcons.Nodes.Class)
                    .withTypeText(declaration.baseName?.let { ": $it" } ?: ""),
            )
        }
    }

    private fun everythingInScope(file: TinyCFile, position: PsiElement, result: CompletionResultSet) {
        val insideFunction = PsiTreeUtil.getParentOfType(position, TinyCBlock::class.java) != null
        val insideClassBody = TinyCScope.enclosingClass(position) != null

        if (!insideFunction) {
            // Functions live only at the top level, and a class body holds
            // fields and methods — so these are the only words that fit.
            for (keyword in if (insideClassBody) listOf("fn") else DECLARATION_WORDS) {
                result.addElement(LookupElementBuilder.create(keyword).bold())
            }
            types(file, result)
            return
        }

        for (declaration in TinyCScope.valuesVisibleAt(position)) {
            val name = declaration.name ?: continue
            result.addElement(
                LookupElementBuilder.create(declaration, name)
                    .withIcon(if (declaration.typeText == null) AllIcons.Nodes.Parameter else AllIcons.Nodes.Variable)
                    .withTypeText(TinyCScope.typeOf(declaration)),
            )
        }

        for (function in file.functions) {
            val name = function.name ?: continue
            result.addElement(
                LookupElementBuilder.create(function, name)
                    .withIcon(AllIcons.Nodes.Method)
                    .withTailText("(" + function.parameters.joinToString(", ") { it.typeText ?: "" } + ")", true)
                    .withTypeText(function.returnTypeText ?: "")
                    .withInsertHandler(
                        if (function.parameters.isEmpty()) ::insertEmptyCall else ::insertCallWithArguments,
                    ),
            )
        }

        // Signatures and all: the compiler exports what it put in its own
        // signature table, so `is_int(string) -> bool` is shown rather than
        // guessed at from the name.
        for (builtin in TinyCTokens.BUILTINS) {
            result.addElement(
                LookupElementBuilder.create(builtin.name)
                    .withIcon(AllIcons.Nodes.Static)
                    .withTypeText(builtin.returns ?: "")
                    .withTailText("(" + builtin.parameters.joinToString(", ") + ")", true)
                    .withInsertHandler(
                        if (builtin.parameters.isEmpty()) ::insertEmptyCall else ::insertCallWithArguments,
                    ),
            )
        }

        for (keyword in STATEMENT_WORDS) {
            result.addElement(LookupElementBuilder.create(keyword).bold())
        }
        types(file, result)
    }

    // -- odds and ends ------------------------------------------------------

    /**
     * The class of the object literal the caret is inside, if it is inside one.
     *
     * The parser leaves an expression as a run of tokens, so this walks back
     * over them: the brace that is still open is the literal's, and the name in
     * front of it is the class.
     */
    private fun objectLiteralClass(file: TinyCFile, position: PsiElement): TinyCClassDecl? {
        val previous = previousMeaningful(position) ?: return null
        val type = previous.node.elementType
        if (type !== TinyCTokens.LBRACE && type !== TinyCTokens.COMMA) return null

        var depth = 0
        var leaf: PsiElement? = previous
        while (leaf != null) {
            when (leaf.node.elementType) {
                TinyCTokens.RBRACE -> depth++
                TinyCTokens.LBRACE -> {
                    if (depth == 0) {
                        val name = previousMeaningful(leaf) ?: return null
                        if (name.node.elementType !== TinyCTokens.IDENTIFIER) return null
                        return TinyCScope.classNamed(file, name.text)
                    }
                    depth--
                }
            }
            leaf = previousMeaningful(leaf)
        }
        return null
    }

    private fun previousMeaningful(element: PsiElement): PsiElement? {
        var leaf = PsiTreeUtil.prevLeaf(element, true)
        while (leaf != null && (leaf.node.elementType === TinyCTokens.LINE_COMMENT || leaf.text.isBlank())) {
            leaf = PsiTreeUtil.prevLeaf(leaf, true)
        }
        return leaf
    }

    /** A call that takes nothing: the caret goes after the parentheses. */
    private fun insertEmptyCall(context: InsertionContext, item: LookupElement) = insertCall(context, after = true)

    /** A call that takes something: the caret goes between the parentheses. */
    private fun insertCallWithArguments(context: InsertionContext, item: LookupElement) =
        insertCall(context, after = false)

    private fun insertCall(context: InsertionContext, after: Boolean) {
        val offset = context.tailOffset
        val text = context.document.charsSequence
        val alreadyThere = offset < text.length && text[offset] == '('
        if (!alreadyThere) context.document.insertString(offset, "()")
        context.editor.caretModel.moveToOffset(offset + if (after) 2 else 1)
    }

    private companion object {
        /**
         * The words that begin a top-level declaration.
         *
         * The one word list here that is not the compiler's: which words *may
         * start* one is a fact about the grammar rather than about the
         * vocabulary, and nothing in `grammar/vocabulary.txt` says it.
         */
        val DECLARATION_WORDS = listOf("fn", "class", "enum")

        /**
         * The words that may begin a statement — everything that shapes a
         * program except the three that declare one, plus the constructs and
         * the two boolean literals.
         *
         * Derived rather than listed, so that a keyword added to the language
         * is offered here the moment the vocabulary is regenerated.
         */
        val STATEMENT_WORDS =
            TinyCTokens.CONTROL_WORDS - DECLARATION_WORDS.toSet() +
                TinyCTokens.CONSTRUCT_WORDS + TinyCTokens.LITERAL_WORDS
    }
}
