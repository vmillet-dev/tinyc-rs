package dev.millet.tinyc.model

import com.intellij.psi.PsiElement
import com.intellij.psi.util.PsiTreeUtil
import dev.millet.tinyc.lang.TinyCElements
import dev.millet.tinyc.psi.TinyCBlock
import dev.millet.tinyc.psi.TinyCClassDecl
import dev.millet.tinyc.psi.TinyCEnumDecl
import dev.millet.tinyc.psi.TinyCFieldDecl
import dev.millet.tinyc.psi.TinyCFile
import dev.millet.tinyc.psi.TinyCFnDecl
import dev.millet.tinyc.psi.TinyCMatchArm
import dev.millet.tinyc.psi.TinyCNamedElement
import dev.millet.tinyc.psi.TinyCParam
import dev.millet.tinyc.psi.TinyCPatternBinding
import dev.millet.tinyc.psi.TinyCVarDecl

/**
 * What is in scope where, worked out from the tree the plugin's own parser
 * built.
 *
 * TinyC makes this small on purpose: a program is one file, functions live only
 * at the top level, there are no globals and no imports. So "everything in
 * scope" is the declarations of this file plus the ones the enclosing function
 * opened before the cursor.
 */
object TinyCScope {

    fun classNamed(file: TinyCFile, name: String?): TinyCClassDecl? =
        name?.let { needle -> file.classes.firstOrNull { it.name == needle } }

    fun enumNamed(file: TinyCFile, name: String?): TinyCEnumDecl? =
        name?.let { needle -> file.enums.firstOrNull { it.name == needle } }

    /** A class's own fields and its bases', the bases' first. */
    fun fieldsOf(file: TinyCFile, cls: TinyCClassDecl): List<TinyCFieldDecl> =
        hierarchy(file, cls).reversed().flatMap { it.fields }

    /**
     * The methods callable on a class: its own, and any a base declared that it
     * did not override. An override keeps the base's slot, so it also keeps the
     * base's name — one entry per name is what a caller sees.
     */
    fun methodsOf(file: TinyCFile, cls: TinyCClassDecl): List<TinyCFnDecl> {
        val seen = LinkedHashMap<String, TinyCFnDecl>()
        // Derived first, so an override is the one that answers for the name.
        for (level in hierarchy(file, cls)) {
            for (method in level.methods) {
                val name = method.name ?: continue
                seen.putIfAbsent(name, method)
            }
        }
        return seen.values.toList()
    }

    /**
     * From the class itself up to its root. A ring cannot happen in a program
     * the compiler accepts, but it can in one being typed, so the walk stops
     * when it sees a class twice rather than looping forever.
     */
    fun hierarchy(file: TinyCFile, cls: TinyCClassDecl): List<TinyCClassDecl> {
        val chain = ArrayList<TinyCClassDecl>()
        var current: TinyCClassDecl? = cls
        while (current != null && chain.none { it === current }) {
            chain.add(current)
            current = classNamed(file, current.baseName)
        }
        return chain
    }

    /**
     * Every name that stands for a value at [position]: the parameters of the
     * enclosing function, the variables declared before it in each enclosing
     * block, and whatever a `match` arm's pattern bound.
     */
    fun valuesVisibleAt(position: PsiElement): List<TinyCNamedElement> {
        val found = ArrayList<TinyCNamedElement>()
        val offset = position.textRange.startOffset
        var element: PsiElement? = position
        while (element != null && element !is TinyCFile) {
            when {
                element is TinyCBlock ->
                    found += PsiTreeUtil.getChildrenOfTypeAsList(element, TinyCVarDecl::class.java)
                        .filter { it.textRange.startOffset < offset }

                element is TinyCFnDecl -> found += element.parameters

                element is TinyCMatchArm ->
                    found += PsiTreeUtil.findChildrenOfType(element, TinyCPatternBinding::class.java)

                element.node.elementType === TinyCElements.FOR_STMT ->
                    found += PsiTreeUtil.getChildrenOfTypeAsList(element, TinyCVarDecl::class.java)
            }
            element = element.parent
        }
        return found
    }

    /** The function a position is written inside, if it is inside one. */
    fun enclosingFunction(position: PsiElement): TinyCFnDecl? =
        PsiTreeUtil.getParentOfType(position, TinyCFnDecl::class.java)

    /** The class a position is written inside, which is what `self` means there. */
    fun enclosingClass(position: PsiElement): TinyCClassDecl? =
        PsiTreeUtil.getParentOfType(position, TinyCClassDecl::class.java)

    /**
     * The class a name stands for at [position] — the receiver of a `.`.
     *
     * `self` is the class the method belongs to; anything else is looked up
     * among the values in scope and then through its written type. A type
     * ending in `[...]` names a collection, and a collection has no members to
     * offer, so it answers nothing.
     */
    fun classOfValue(file: TinyCFile, position: PsiElement, name: String): TinyCClassDecl? {
        if (name == "self") return enclosingClass(position)
        val declaration = valuesVisibleAt(position).firstOrNull { it.name == name } ?: return null
        val written = declaration.typeText ?: return null
        if (written.contains('[')) return null
        return classNamed(file, written)
    }

    /** The written type of a name in scope, for a completion popup to show. */
    fun typeOf(declaration: TinyCNamedElement): String = when (declaration) {
        is TinyCParam -> declaration.typeText ?: "self"
        else -> declaration.typeText ?: ""
    }
}
