package dev.millet.tinyc.annotator

import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.execution.process.CapturingProcessHandler
import com.intellij.lang.annotation.AnnotationHolder
import com.intellij.lang.annotation.ExternalAnnotator
import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.openapi.diagnostic.logger
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.TextRange
import com.intellij.psi.PsiDocumentManager
import com.intellij.psi.PsiFile
import dev.millet.tinyc.psi.TinyCFile
import dev.millet.tinyc.settings.TinyCSettings
import dev.millet.tinyc.toolchain.TinyCToolchain
import java.nio.file.Files
import java.nio.file.Path

/** Everything `doAnnotate` needs, taken while the read lock is held. */
data class TinyCCompileRequest(
    val project: Project,
    val source: Path?,
    val text: String,
    val optimise: Boolean,
)

/**
 * The mistakes in the editor are the compiler's own.
 *
 * Nothing here decides whether a program is correct: the text in the editor is
 * handed to `tinyc`, and what comes back is placed on the lines it names. That
 * is what makes the editor agree with the terminal by construction — including
 * about the things only the whole compiler knows, like an index that is out of
 * range or a `%d` handed a string.
 *
 * The pipeline is stopped after the IR, which is the last stage that can refuse
 * a program; there is no point generating code nobody will run.
 */
class TinyCExternalAnnotator : ExternalAnnotator<TinyCCompileRequest, List<TinyCDiagnostic>>() {

    override fun collectInformation(file: PsiFile, editor: Editor, hasErrors: Boolean): TinyCCompileRequest? {
        if (file !is TinyCFile) return null
        val project = file.project
        val settings = TinyCSettings.of(project).options
        if (!settings.annotate) return null
        val path = file.virtualFile?.path?.let { runCatching { Path.of(it) }.getOrNull() }
        return TinyCCompileRequest(project, path, file.text, settings.optimiseWhileAnnotating)
    }

    override fun doAnnotate(request: TinyCCompileRequest): List<TinyCDiagnostic> {
        val command = TinyCToolchain.of(request.project).compilerCommand(request.source) ?: return emptyList()

        // The compiler reads a file, and what is being typed is not on disk
        // yet — so it goes to a scratch copy. Nothing else is read from it: the
        // diagnostics are placed by line and column, not by path.
        val scratch = try {
            Files.createTempFile("tinyc-editor-", ".tc")
        } catch (e: Exception) {
            LOG.warn("could not make a scratch file for the annotator", e)
            return emptyList()
        }

        return try {
            Files.writeString(scratch, request.text)
            val line: GeneralCommandLine = command
                .withParameters("--emit", "ir")
                .withCharset(Charsets.UTF_8)
            if (!request.optimise) line.addParameter("--no-optimise")
            line.addParameter(scratch.toString())

            val output = CapturingProcessHandler(line).runProcess(TIMEOUT_MS)
            if (output.isTimeout) emptyList() else TinyCDiagnostics.parse(output.stderr)
        } catch (e: Exception) {
            LOG.warn("running the compiler for diagnostics failed", e)
            emptyList()
        } finally {
            runCatching { Files.deleteIfExists(scratch) }
        }
    }

    override fun apply(file: PsiFile, results: List<TinyCDiagnostic>, holder: AnnotationHolder) {
        val document = PsiDocumentManager.getInstance(file.project).getDocument(file) ?: return
        for (diagnostic in results) {
            val range = rangeOf(document.charsSequence, document, diagnostic) ?: continue
            holder.newAnnotation(HighlightSeverity.ERROR, message(diagnostic))
                .range(range)
                .tooltip(tooltip(diagnostic))
                .create()
        }
    }

    private fun message(diagnostic: TinyCDiagnostic): String =
        listOfNotNull(diagnostic.message, diagnostic.label).joinToString(" — ")

    private fun tooltip(diagnostic: TinyCDiagnostic): String {
        val parts = ArrayList<String>()
        parts += escape(diagnostic.message)
        diagnostic.label?.let { parts += escape(it) }
        diagnostic.note?.let { parts += "<i>note: " + escape(it) + "</i>" }
        return parts.joinToString("<br>")
    }

    private fun escape(text: String): String =
        text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

    /**
     * Where a diagnostic lands in this document.
     *
     * The compiler counts columns in *characters* so that an accent does not
     * shift them; Java counts in UTF-16 units. Anything outside the basic plane
     * — an emoji in a string literal — is two units and one character, so the
     * two have to be walked rather than added.
     */
    private fun rangeOf(
        text: CharSequence,
        document: com.intellij.openapi.editor.Document,
        diagnostic: TinyCDiagnostic,
    ): TextRange? {
        val index = diagnostic.line - 1
        if (index < 0 || index >= document.lineCount) return null
        val lineStart = document.getLineStartOffset(index)
        val lineEnd = document.getLineEndOffset(index)

        val start = advance(text, lineStart, lineEnd, diagnostic.column - 1)
        val end = advance(text, start, lineEnd, diagnostic.length).coerceAtLeast(
            (start + 1).coerceAtMost(document.textLength),
        )
        if (start >= document.textLength) return null
        return TextRange(start, end.coerceAtMost(document.textLength))
    }

    /** Move `characters` characters on from `from`, stopping at `limit`. */
    private fun advance(text: CharSequence, from: Int, limit: Int, characters: Int): Int {
        var offset = from
        var left = characters
        while (left > 0 && offset < limit) {
            offset += Character.charCount(Character.codePointAt(text, offset))
            left--
        }
        return offset.coerceAtMost(limit)
    }

    private companion object {
        val LOG = logger<TinyCExternalAnnotator>()
        const val TIMEOUT_MS = 10_000
    }
}
