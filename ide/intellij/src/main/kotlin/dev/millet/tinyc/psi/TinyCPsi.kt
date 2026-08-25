package dev.millet.tinyc.psi

import com.intellij.extapi.psi.ASTWrapperPsiElement
import com.intellij.extapi.psi.PsiFileBase
import com.intellij.lang.ASTNode
import com.intellij.openapi.fileTypes.FileType
import com.intellij.psi.FileViewProvider
import com.intellij.psi.PsiElement
import com.intellij.psi.PsiNamedElement
import com.intellij.psi.util.PsiTreeUtil
import com.intellij.util.IncorrectOperationException
import dev.millet.tinyc.lang.TinyCElements
import dev.millet.tinyc.lang.TinyCFileType
import dev.millet.tinyc.lang.TinyCLanguage
import dev.millet.tinyc.lang.TinyCTokens

class TinyCFile(viewProvider: FileViewProvider) : PsiFileBase(viewProvider, TinyCLanguage) {
    override fun getFileType(): FileType = TinyCFileType
    override fun toString(): String = "TinyC file"

    val functions: List<TinyCFnDecl>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCFnDecl::class.java)

    val classes: List<TinyCClassDecl>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCClassDecl::class.java)

    val enums: List<TinyCEnumDecl>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCEnumDecl::class.java)
}

/** Anything that gives a name to something. */
abstract class TinyCNamedElement(node: ASTNode) : ASTWrapperPsiElement(node), PsiNamedElement {
    /** The identifier token this declaration named, if it got that far. */
    val nameIdentifier: PsiElement?
        get() = node.findChildByType(TinyCTokens.IDENTIFIER)?.psi

    override fun getName(): String? = nameIdentifier?.text

    override fun setName(name: String): PsiElement =
        throw IncorrectOperationException("renaming is not supported yet")

    /** The type written in front of the name, as it was spelled. */
    open val typeText: String?
        get() = PsiTreeUtil.getChildOfType(this, TinyCTypeRef::class.java)?.text?.trim()
}

class TinyCFnDecl(node: ASTNode) : TinyCNamedElement(node) {
    val parameters: List<TinyCParam>
        get() = PsiTreeUtil.findChildrenOfType(paramList, TinyCParam::class.java).toList()

    private val paramList: PsiElement?
        get() = node.findChildByType(TinyCElements.PARAM_LIST)?.psi

    val returnTypeText: String?
        get() = node.findChildByType(TinyCElements.RET_TYPE)?.psi?.text?.trim()

    val body: TinyCBlock?
        get() = PsiTreeUtil.getChildOfType(this, TinyCBlock::class.java)

    /** A method is a function written inside a class body. */
    val owningClass: TinyCClassDecl?
        get() = parent as? TinyCClassDecl

    /** `fn area(self) -> int`, for a completion popup or a structure view row. */
    fun signature(): String {
        val params = parameters.joinToString(", ") { listOfNotNull(it.typeText, it.name).joinToString(" ") }
        val ret = returnTypeText?.let { " -> $it" } ?: ""
        return "${name ?: "?"}($params)$ret"
    }
}

class TinyCParam(node: ASTNode) : TinyCNamedElement(node)

class TinyCVarDecl(node: ASTNode) : TinyCNamedElement(node)

class TinyCFieldDecl(node: ASTNode) : TinyCNamedElement(node)

class TinyCPatternBinding(node: ASTNode) : TinyCNamedElement(node) {
    override val typeText: String? get() = null
}

class TinyCClassDecl(node: ASTNode) : TinyCNamedElement(node) {
    /** The class this one derives from, as written after the colon. */
    val baseName: String?
        get() = node.findChildByType(TinyCElements.BASE_CLASS)?.psi?.text?.trim()

    val fields: List<TinyCFieldDecl>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCFieldDecl::class.java)

    val methods: List<TinyCFnDecl>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCFnDecl::class.java)

    override val typeText: String? get() = null
}

class TinyCEnumDecl(node: ASTNode) : TinyCNamedElement(node) {
    val variants: List<TinyCEnumVariant>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCEnumVariant::class.java)

    override val typeText: String? get() = null
}

class TinyCEnumVariant(node: ASTNode) : TinyCNamedElement(node) {
    val owningEnum: TinyCEnumDecl?
        get() = parent as? TinyCEnumDecl

    /** The types the variant carries, if it carries any. */
    val payload: List<String>
        get() = PsiTreeUtil.getChildrenOfTypeAsList(this, TinyCTypeRef::class.java).map { it.text.trim() }

    override val typeText: String? get() = null
}

class TinyCTypeRef(node: ASTNode) : ASTWrapperPsiElement(node) {
    /** `Circle` for `Circle`, `int` for `int[3]` — the name without its shape. */
    val baseName: String
        get() = node.findChildByType(TinyCTokens.IDENTIFIER)?.text
            ?: text.takeWhile { it.isLetterOrDigit() || it == '_' }

    val isArrayOrList: Boolean
        get() = text.contains('[')
}

class TinyCBlock(node: ASTNode) : ASTWrapperPsiElement(node)

class TinyCMatchArm(node: ASTNode) : ASTWrapperPsiElement(node)

/** Everything with no behaviour of its own. */
class TinyCPlainElement(node: ASTNode) : ASTWrapperPsiElement(node)
