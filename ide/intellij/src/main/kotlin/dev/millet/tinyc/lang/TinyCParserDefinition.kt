package dev.millet.tinyc.lang

import com.intellij.lang.ASTNode
import com.intellij.lang.ParserDefinition
import com.intellij.lang.PsiParser
import com.intellij.lexer.Lexer
import com.intellij.openapi.project.Project
import com.intellij.psi.FileViewProvider
import com.intellij.psi.PsiElement
import com.intellij.psi.PsiFile
import com.intellij.psi.TokenType
import com.intellij.psi.tree.IFileElementType
import com.intellij.psi.tree.TokenSet
import dev.millet.tinyc.psi.TinyCFile

class TinyCParserDefinition : ParserDefinition {
    override fun createLexer(project: Project?): Lexer = TinyCLexer()
    override fun createParser(project: Project?): PsiParser = TinyCParser()
    override fun getFileNodeType(): IFileElementType = FILE
    override fun getWhitespaceTokens(): TokenSet = WHITESPACE
    override fun getCommentTokens(): TokenSet = TinyCTokens.COMMENTS
    override fun getStringLiteralElements(): TokenSet = TinyCTokens.STRINGS
    override fun createElement(node: ASTNode): PsiElement = TinyCElements.createPsi(node)
    override fun createFile(viewProvider: FileViewProvider): PsiFile = TinyCFile(viewProvider)

    companion object {
        val FILE = IFileElementType(TinyCLanguage)
        private val WHITESPACE = TokenSet.create(TokenType.WHITE_SPACE)
    }
}
