package com.infrawrench.opencomputeruse.host;

/** A failed tool call, said to the caller as "Error: <message>". */
final class ToolError extends Exception {
    ToolError(String message) {
        super(message);
    }
}
