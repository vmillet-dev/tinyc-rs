package dev.millet.tinyc.run

import com.intellij.execution.process.ProcessHandler
import com.intellij.execution.process.ProcessOutputTypes
import java.io.OutputStream

/**
 * A run that is only a message.
 *
 * When the build fails there is no program to start, and when *Build only* is
 * ticked there is one but nobody asked for it — either way the console should
 * still open and show what the tools said, so that a diagnostic is a link and
 * not a balloon that disappears. So this stands in for a process: it says its
 * piece and terminates.
 */
class TinyCMessageProcessHandler(private val text: String, private val failed: Boolean) : ProcessHandler() {

    override fun startNotify() {
        super.startNotify()
        notifyTextAvailable(text, if (failed) ProcessOutputTypes.STDERR else ProcessOutputTypes.SYSTEM)
        notifyProcessTerminated(if (failed) 1 else 0)
    }

    override fun destroyProcessImpl() = notifyProcessTerminated(if (failed) 1 else 0)

    override fun detachProcessImpl() = notifyProcessDetached()

    override fun detachIsDefault(): Boolean = false

    override fun getProcessInput(): OutputStream? = null
}
