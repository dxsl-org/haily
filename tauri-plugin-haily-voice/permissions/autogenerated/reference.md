## Default Permission

Default permissions for the haily-voice plugin — grants every command. Internal, single-consumer plugin (only `src-tauri-mobile` depends on it), so per-command allow/deny granularity is YAGNI; a published plugin would split these.

#### This default permission set includes the following:

- `allow-start-stt`
- `allow-stop-stt`
- `allow-speak-chunk`
- `allow-stop-speaking`
- `allow-tts-state`
- `allow-check-permissions`
- `allow-request-permissions`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`haily-voice:allow-check-permissions`

</td>
<td>

Enables the check_permissions command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-check-permissions`

</td>
<td>

Denies the check_permissions command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:allow-request-permissions`

</td>
<td>

Enables the request_permissions command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-request-permissions`

</td>
<td>

Denies the request_permissions command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:allow-speak-chunk`

</td>
<td>

Enables the speak_chunk command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-speak-chunk`

</td>
<td>

Denies the speak_chunk command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:allow-start-stt`

</td>
<td>

Enables the start_stt command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-start-stt`

</td>
<td>

Denies the start_stt command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:allow-stop-speaking`

</td>
<td>

Enables the stop_speaking command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-stop-speaking`

</td>
<td>

Denies the stop_speaking command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:allow-stop-stt`

</td>
<td>

Enables the stop_stt command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-stop-stt`

</td>
<td>

Denies the stop_stt command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:allow-tts-state`

</td>
<td>

Enables the tts_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`haily-voice:deny-tts-state`

</td>
<td>

Denies the tts_state command without any pre-configured scope.

</td>
</tr>
</table>
