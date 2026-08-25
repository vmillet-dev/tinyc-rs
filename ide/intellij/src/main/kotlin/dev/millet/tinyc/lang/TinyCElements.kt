package dev.millet.tinyc.lang

import com.intellij.lang.ASTNode
import com.intellij.psi.PsiElement
import com.intellij.psi.tree.IElementType
import dev.millet.tinyc.psi.TinyCBlock
import dev.millet.tinyc.psi.TinyCClassDecl
import dev.millet.tinyc.psi.TinyCEnumDecl
import dev.millet.tinyc.psi.TinyCEnumVariant
import dev.millet.tinyc.psi.TinyCFieldDecl
import dev.millet.tinyc.psi.TinyCFnDecl
import dev.millet.tinyc.psi.TinyCMatchArm
import dev.millet.tinyc.psi.TinyCParam
import dev.millet.tinyc.psi.TinyCPatternBinding
import dev.millet.tinyc.psi.TinyCPlainElement
import dev.millet.tinyc.psi.TinyCTypeRef
import dev.millet.tinyc.psi.TinyCVarDecl

class TinyCElementType(debugName: String) : IElementType(debugName, TinyCLanguage) {
    override fun toString(): String = "TinyC:" + super.toString()
}

/**
 * The shapes the parser builds.
 *
 * The tree is deliberately shallower than the compiler's: it goes as far as
 * *what is declared where*, which is everything completion and the structure
 * view ask of it, and treats an expression as a run of tokens. Type checking
 * is not repeated here — `tinyc` itself does that, and says so in the editor
 * through the external annotator.
 */
object TinyCElements {
    val FN_DECL = TinyCElementType("FN_DECL")
    val PARAM_LIST = TinyCElementType("PARAM_LIST")
    val PARAM = TinyCElementType("PARAM")
    val RET_TYPE = TinyCElementType("RET_TYPE")
    val TYPE_REF = TinyCElementType("TYPE_REF")
    val BLOCK = TinyCElementType("BLOCK")

    val CLASS_DECL = TinyCElementType("CLASS_DECL")
    val BASE_CLASS = TinyCElementType("BASE_CLASS")
    val FIELD_DECL = TinyCElementType("FIELD_DECL")

    val ENUM_DECL = TinyCElementType("ENUM_DECL")
    val ENUM_VARIANT = TinyCElementType("ENUM_VARIANT")

    val VAR_DECL = TinyCElementType("VAR_DECL")
    val IF_STMT = TinyCElementType("IF_STMT")
    val WHILE_STMT = TinyCElementType("WHILE_STMT")
    val FOR_STMT = TinyCElementType("FOR_STMT")
    val RETURN_STMT = TinyCElementType("RETURN_STMT")
    val JUMP_STMT = TinyCElementType("JUMP_STMT")
    val EXPR_STMT = TinyCElementType("EXPR_STMT")
    val MATCH = TinyCElementType("MATCH")
    val MATCH_ARM = TinyCElementType("MATCH_ARM")
    val PATTERN = TinyCElementType("PATTERN")
    val PATTERN_BINDING = TinyCElementType("PATTERN_BINDING")

    fun createPsi(node: ASTNode): PsiElement = when (node.elementType) {
        FN_DECL -> TinyCFnDecl(node)
        PARAM -> TinyCParam(node)
        VAR_DECL -> TinyCVarDecl(node)
        CLASS_DECL -> TinyCClassDecl(node)
        FIELD_DECL -> TinyCFieldDecl(node)
        ENUM_DECL -> TinyCEnumDecl(node)
        ENUM_VARIANT -> TinyCEnumVariant(node)
        PATTERN_BINDING -> TinyCPatternBinding(node)
        TYPE_REF -> TinyCTypeRef(node)
        BLOCK -> TinyCBlock(node)
        MATCH_ARM -> TinyCMatchArm(node)
        else -> TinyCPlainElement(node)
    }
}
