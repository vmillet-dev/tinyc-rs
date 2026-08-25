package dev.millet.tinyc.run

import com.intellij.execution.actions.ConfigurationContext
import com.intellij.execution.actions.LazyRunConfigurationProducer
import com.intellij.execution.configurations.ConfigurationFactory
import com.intellij.execution.filters.ConsoleFilterProvider
import com.intellij.execution.filters.Filter
import com.intellij.execution.filters.OpenFileHyperlinkInfo
import com.intellij.execution.lineMarker.ExecutorAction
import com.intellij.execution.lineMarker.RunLineMarkerContributor
import com.intellij.icons.AllIcons
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Ref
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.psi.PsiElement
import dev.millet.tinyc.lang.TinyCFileType
import dev.millet.tinyc.lang.TinyCTokens
import dev.millet.tinyc.psi.TinyCFnDecl
import java.nio.file.Path

/**
 * Right-clicking a `.tc` file — or the gutter icon beside `fn main` — makes a
 * run configuration for it, so nothing has to be filled in by hand.
 */
class TinyCRunConfigurationProducer : LazyRunConfigurationProducer<TinyCRunConfiguration>() {

    override fun getConfigurationFactory(): ConfigurationFactory = TinyCRunConfigurationType.factory()

    override fun setupConfigurationFromContext(
        configuration: TinyCRunConfiguration,
        context: ConfigurationContext,
        sourceElement: Ref<PsiElement>,
    ): Boolean {
        val file = context.location?.virtualFile ?: return false
        if (file.fileType != TinyCFileType) return false
        configuration.sourceFile = file.path
        configuration.name = file.name
        return true
    }

    override fun isConfigurationFromContext(
        configuration: TinyCRunConfiguration,
        context: ConfigurationContext,
    ): Boolean {
        val file = context.location?.virtualFile ?: return false
        return configuration.sourceFile == file.path
    }
}

/** The green arrow in the gutter, next to the one function a program starts at. */
class TinyCRunLineMarkerContributor : RunLineMarkerContributor() {
    override fun getInfo(element: PsiElement): Info? {
        if (element.node.elementType !== TinyCTokens.IDENTIFIER) return null
        if (element.text != "main") return null
        val function = element.parent as? TinyCFnDecl ?: return null
        if (function.nameIdentifier !== element || function.owningClass != null) return null
        return Info(
            AllIcons.RunConfigurations.TestState.Run,
            ExecutorAction.getActions(0),
            { "Run " + element.containingFile.name },
        )
    }
}

/**
 * Turns the `file:line:column` of a diagnostic into a link.
 *
 * The compiler echoes the path it was given, which the run configuration keeps
 * relative to the working directory — so a relative one is resolved against
 * that. The `-->` form is matched first and separately, because a path written
 * out in full may have spaces in it and the bare form cannot tell where such a
 * path begins.
 */
class TinyCConsoleFilter(private val project: Project, private val from: Path?) : Filter {

    override fun applyFilter(line: String, entireLength: Int): Filter.Result? {
        val match = POINTED_AT.find(line) ?: NAMED.find(line) ?: return null
        val (path, lineNumber, column) = match.destructured

        val resolved = runCatching {
            val candidate = Path.of(path.trim())
            if (candidate.isAbsolute || from == null) candidate else from.resolve(candidate)
        }.getOrNull() ?: return null

        val file = LocalFileSystem.getInstance().findFileByPath(resolved.normalize().toString().replace('\\', '/'))
            ?: return null

        val start = entireLength - line.length
        val range = match.groups[1]!!.range.first..match.groups[3]!!.range.last
        return Filter.Result(
            start + range.first,
            start + range.last + 1,
            OpenFileHyperlinkInfo(project, file, lineNumber.toInt() - 1, column.toInt() - 1),
        )
    }

    private companion object {
        val POINTED_AT = Regex("""-->\s+(.+?):(\d+):(\d+)\s*$""")
        val NAMED = Regex("""(\S+\.tc):(\d+):(\d+)""")
    }
}

/** The same links in any console, including the one `cargo run` writes to. */
class TinyCConsoleFilterProvider : ConsoleFilterProvider {
    override fun getDefaultFilters(project: Project): Array<Filter> {
        val base = project.basePath?.let { runCatching { Path.of(it) }.getOrNull() }
        return arrayOf(TinyCConsoleFilter(project, base))
    }
}
