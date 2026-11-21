// 复制按钮：复制同一块里 <code> 的文字，按钮上的字换成「已复制」两秒。
document.querySelectorAll(".copy").forEach((btn) => {
	const label = btn.textContent;
	btn.addEventListener("click", async () => {
		const text = btn.parentElement.querySelector("code").textContent.trim();
		try {
			await navigator.clipboard.writeText(text);
		} catch {
			const area = document.createElement("textarea");
			area.value = text;
			document.body.append(area);
			area.select();
			document.execCommand("copy");
			area.remove();
		}
		btn.textContent = btn.dataset.done;
		setTimeout(() => { btn.textContent = label; }, 2000);
	});
});
