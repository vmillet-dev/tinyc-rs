package dev.millet.tinyc.lang

import com.intellij.openapi.editor.colors.TextAttributesKey
import com.intellij.openapi.fileTypes.SyntaxHighlighter
import com.intellij.openapi.options.colors.AttributesDescriptor
import com.intellij.openapi.options.colors.ColorDescriptor
import com.intellij.openapi.options.colors.ColorSettingsPage
import javax.swing.Icon

class TinyCColorSettingsPage : ColorSettingsPage {
    override fun getDisplayName(): String = "TinyC"
    override fun getIcon(): Icon = TinyCIcons.FILE
    override fun getHighlighter(): SyntaxHighlighter = TinyCSyntaxHighlighter()
    override fun getColorDescriptors(): Array<ColorDescriptor> = ColorDescriptor.EMPTY_ARRAY
    override fun getAttributeDescriptors(): Array<AttributesDescriptor> = DESCRIPTORS

    override fun getAdditionalHighlightingTagToDescriptorMap(): Map<String, TextAttributesKey> = mapOf(
        "class" to TinyCColors.CLASS_NAME,
        "field" to TinyCColors.FIELD,
        "param" to TinyCColors.PARAMETER,
        "fn" to TinyCColors.FUNCTION_DECLARATION,
        "call" to TinyCColors.FUNCTION_CALL,
        "method" to TinyCColors.METHOD_CALL,
        "builtin" to TinyCColors.BUILTIN,
        "variant" to TinyCColors.ENUM_VARIANT,
        "format" to TinyCColors.FORMAT_SPECIFIER,
    )

    override fun getDemoText(): String = """
        // Enums are matched exhaustively, with no catch-all.
        enum <class>Colour</class> { <variant>Red</variant>, <variant>Green</variant> }

        class <class>Shape</class> {
          int <field>sides</field>;
          fn <fn>area</fn>(<builtin>self</builtin>) -> int {
            return 0;
          }
        }

        class <class>Rect</class> : <class>Shape</class> {
          int <field>w</field>;
          int <field>h</field>;
          fn <fn>area</fn>(<builtin>self</builtin>) -> int {
            return <builtin>self</builtin>.<field>w</field> * <builtin>self</builtin>.<field>h</field>;
          }
        }

        fn <fn>describe</fn>(<class>Colour</class> <param>c</param>) -> string {
          return match (<param>c</param>) {
            <class>Colour</class>::<variant>Red</variant> => "warm",
            <class>Colour</class>::<variant>Green</variant> => "cool",
          };
        }

        fn <fn>main</fn>() {
          <class>Rect</class> r = <class>Rect</class> { <field>sides</field>: 4, <field>w</field>: 3, <field>h</field>: 7 };
          int[] xs = [];
          push(xs, r.<method>area</method>());
          for (int i = 0; i < len(xs); i = i + 1) {
            println("<format>%s</format> area <format>%d</format>", <call>describe</call>(<class>Colour</class>::<variant>Red</variant>), xs[i]);
          }
          if (<builtin>eof</builtin>()) {
            print('!');
          }
        }
    """.trimIndent()

    private companion object {
        val DESCRIPTORS = arrayOf(
            AttributesDescriptor("Keyword", TinyCColors.KEYWORD),
            AttributesDescriptor("Type name", TinyCColors.TYPE),
            AttributesDescriptor("Construct//print, println, len, push", TinyCColors.CONSTRUCT),
            AttributesDescriptor("Built-in function", TinyCColors.BUILTIN),
            AttributesDescriptor("Comment", TinyCColors.COMMENT),
            AttributesDescriptor("Number", TinyCColors.NUMBER),
            AttributesDescriptor("String", TinyCColors.STRING),
            AttributesDescriptor("Character", TinyCColors.CHAR),
            AttributesDescriptor("Format specifier", TinyCColors.FORMAT_SPECIFIER),
            AttributesDescriptor("Identifier", TinyCColors.IDENTIFIER),
            AttributesDescriptor("Declarations//Function", TinyCColors.FUNCTION_DECLARATION),
            AttributesDescriptor("Declarations//Class or enum", TinyCColors.CLASS_NAME),
            AttributesDescriptor("Declarations//Field", TinyCColors.FIELD),
            AttributesDescriptor("Declarations//Parameter", TinyCColors.PARAMETER),
            AttributesDescriptor("Declarations//Enum variant", TinyCColors.ENUM_VARIANT),
            AttributesDescriptor("Calls//Function call", TinyCColors.FUNCTION_CALL),
            AttributesDescriptor("Calls//Method call", TinyCColors.METHOD_CALL),
            AttributesDescriptor("Braces and operators//Operator", TinyCColors.OPERATOR),
            AttributesDescriptor("Braces and operators//Parentheses", TinyCColors.PARENTHESES),
            AttributesDescriptor("Braces and operators//Braces", TinyCColors.BRACES),
            AttributesDescriptor("Braces and operators//Brackets", TinyCColors.BRACKETS),
            AttributesDescriptor("Braces and operators//Semicolon", TinyCColors.SEMICOLON),
            AttributesDescriptor("Braces and operators//Comma", TinyCColors.COMMA),
            AttributesDescriptor("Braces and operators//Dot", TinyCColors.DOT),
            AttributesDescriptor("Bad character", TinyCColors.BAD_CHARACTER),
        )
    }
}
