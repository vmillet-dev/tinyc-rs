package dev.millet.tinyc.lang

import com.intellij.ide.projectView.PresentationData
import com.intellij.ide.structureView.StructureViewBuilder
import com.intellij.ide.structureView.StructureViewModel
import com.intellij.ide.structureView.StructureViewModelBase
import com.intellij.ide.structureView.StructureViewTreeElement
import com.intellij.ide.structureView.TreeBasedStructureViewBuilder
import com.intellij.ide.util.treeView.smartTree.SortableTreeElement
import com.intellij.ide.util.treeView.smartTree.TreeElement
import com.intellij.icons.AllIcons
import com.intellij.lang.PsiStructureViewFactory
import com.intellij.navigation.ItemPresentation
import com.intellij.openapi.editor.Editor
import com.intellij.psi.NavigatablePsiElement
import com.intellij.psi.PsiFile
import dev.millet.tinyc.psi.TinyCClassDecl
import dev.millet.tinyc.psi.TinyCEnumDecl
import dev.millet.tinyc.psi.TinyCEnumVariant
import dev.millet.tinyc.psi.TinyCFieldDecl
import dev.millet.tinyc.psi.TinyCFile
import dev.millet.tinyc.psi.TinyCFnDecl

class TinyCStructureViewFactory : PsiStructureViewFactory {
    override fun getStructureViewBuilder(psiFile: PsiFile): StructureViewBuilder? {
        if (psiFile !is TinyCFile) return null
        return object : TreeBasedStructureViewBuilder() {
            override fun createStructureViewModel(editor: Editor?): StructureViewModel =
                TinyCStructureViewModel(psiFile)
        }
    }
}

private class TinyCStructureViewModel(file: TinyCFile) :
    StructureViewModelBase(file, TinyCStructureElement(file)), StructureViewModel.ElementInfoProvider {

    init {
        withSuitableClasses(
            TinyCFnDecl::class.java,
            TinyCClassDecl::class.java,
            TinyCEnumDecl::class.java,
            TinyCFieldDecl::class.java,
            TinyCEnumVariant::class.java,
        )
    }

    override fun isAlwaysShowsPlus(element: StructureViewTreeElement): Boolean =
        element.value is TinyCFile || element.value is TinyCClassDecl

    override fun isAlwaysLeaf(element: StructureViewTreeElement): Boolean =
        element.value is TinyCFieldDecl || element.value is TinyCEnumVariant
}

private class TinyCStructureElement(private val element: NavigatablePsiElement) :
    StructureViewTreeElement, SortableTreeElement {

    override fun getValue(): Any = element
    override fun navigate(requestFocus: Boolean) = element.navigate(requestFocus)
    override fun canNavigate(): Boolean = element.canNavigate()
    override fun canNavigateToSource(): Boolean = element.canNavigateToSource()
    override fun getAlphaSortKey(): String = element.name ?: ""

    override fun getPresentation(): ItemPresentation = when (val target = element) {
        is TinyCFnDecl -> PresentationData(
            target.signature(),
            target.owningClass?.name,
            AllIcons.Nodes.Method,
            null,
        )

        is TinyCClassDecl -> PresentationData(
            target.name.orEmpty(),
            target.baseName?.let { ": $it" },
            AllIcons.Nodes.Class,
            null,
        )

        is TinyCEnumDecl -> PresentationData(target.name.orEmpty(), null, AllIcons.Nodes.Enum, null)

        is TinyCFieldDecl -> PresentationData(
            target.name.orEmpty(),
            target.typeText,
            AllIcons.Nodes.Field,
            null,
        )

        is TinyCEnumVariant -> PresentationData(
            target.name.orEmpty() + target.payload.let { if (it.isEmpty()) "" else "(" + it.joinToString(", ") + ")" },
            null,
            AllIcons.Nodes.Enum,
            null,
        )

        else -> PresentationData(target.name.orEmpty(), null, AllIcons.FileTypes.Text, null)
    }

    override fun getChildren(): Array<TreeElement> {
        val children: List<NavigatablePsiElement> = when (val target = element) {
            is TinyCFile -> target.classes + target.enums + target.functions
            is TinyCClassDecl -> target.fields + target.methods
            is TinyCEnumDecl -> target.variants
            else -> emptyList()
        }
        return children.map { TinyCStructureElement(it) }.toTypedArray()
    }
}
