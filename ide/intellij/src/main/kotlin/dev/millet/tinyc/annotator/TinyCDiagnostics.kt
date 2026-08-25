package dev.millet.tinyc.annotator

/**
 * One thing the compiler refused, placed where it was written.
 *
 * [length] is in characters, as the compiler counts them — that is what the row
 * of carets under the echoed line measures. [label] is what the compiler wrote
 * beside those carets, and [note] the sentence it added underneath.
 */
data class TinyCDiagnostic(
    val line: Int,
    val column: Int,
    val length: Int,
    val message: String,
    val label: String? = null,
    val note: String? = null,
)

/**
 * Reading `tinyc`'s own output back.
 *
 * The compiler prints a message, a line, a column and a caret under the source;
 * that rendering is the one shape every stage goes through (`SourceFile::render`),
 * so this parses it rather than asking the compiler for a machine format it does
 * not have. A message with no `-->` line — a file that could not be read, a
 * target that does not exist — is about the run rather than about the program,
 * and is left out.
 */
object TinyCDiagnostics {
    private val LOCATION = Regex("""^\s*-->\s*(.+):(\d+):(\d+)\s*$""")
    private val CARETS = Regex("""^\s*\|\s*(\^+)(.*)$""")
    private val NOTE = Regex("""^\s*=\s*note:\s*(.*)$""")

    fun parse(output: String): List<TinyCDiagnostic> {
        val found = ArrayList<TinyCDiagnostic>()
        var message: String? = null
        var label: String? = null
        var note: String? = null
        var line = 0
        var column = 0
        var length = 0
        var located = false

        fun flush() {
            val text = message ?: return
            if (located) {
                found += TinyCDiagnostic(line, column, length.coerceAtLeast(1), text, label, note)
            }
            message = null
            label = null
            note = null
            located = false
            length = 0
        }

        for (raw in output.lineSequence()) {
            val text = raw.trimEnd()
            when {
                text.startsWith("error: ") -> {
                    flush()
                    message = text.removePrefix("error: ").trim()
                }

                message == null -> Unit

                // The first location is the diagnostic's own. A note may carry a
                // second one, pointing at what it is comparing against.
                !located && LOCATION.matches(text) -> {
                    val match = LOCATION.find(text)!!
                    line = match.groupValues[2].toInt()
                    column = match.groupValues[3].toInt()
                    located = true
                }

                located && length == 0 && CARETS.matches(text) -> {
                    val match = CARETS.find(text)!!
                    length = match.groupValues[1].length
                    label = match.groupValues[2].trim().ifEmpty { null }
                }

                NOTE.matches(text) -> note = NOTE.find(text)!!.groupValues[1].trim()
            }
        }
        flush()
        return found
    }
}
